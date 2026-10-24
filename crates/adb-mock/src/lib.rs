//! A fake ADB server speaking enough of the smart-socket protocol to test
//! the client and the device manager without real phones.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[derive(Debug, Clone)]
pub struct MockDevice {
    pub serial: String,
    pub state: String,
    pub model: String,
    pub usb: Option<String>,
    pub props: HashMap<String, String>,
    pub features: Vec<String>,
    /// Exact command → (stdout, exit code).
    pub shell: HashMap<String, (String, u8)>,
}

impl MockDevice {
    pub fn new(serial: &str, model: &str) -> Self {
        let mut props = HashMap::new();
        props.insert("ro.product.model".to_owned(), model.to_owned());
        props.insert("ro.build.version.sdk".to_owned(), "35".to_owned());
        Self {
            serial: serial.to_owned(),
            state: "device".to_owned(),
            model: model.to_owned(),
            usb: None,
            props,
            features: vec!["shell_v2".into(), "cmd".into(), "stat_v2".into(), "abb_exec".into()],
            shell: HashMap::new(),
        }
    }

    pub fn prop(mut self, key: &str, value: &str) -> Self {
        self.props.insert(key.to_owned(), value.to_owned());
        self
    }

    pub fn usb(mut self, devpath: &str) -> Self {
        self.usb = Some(devpath.to_owned());
        self
    }

    pub fn on_shell(mut self, cmd: &str, stdout: &str, exit: u8) -> Self {
        self.shell.insert(cmd.to_owned(), (stdout.to_owned(), exit));
        self
    }
}

#[derive(Debug, Default)]
pub struct MockState {
    pub devices: Vec<MockDevice>,
    /// Devices that `host:connect:<addr>` will bring online, keyed by addr.
    pub connectable: HashMap<String, MockDevice>,
    /// `addr` → (expected code, guid reported on success).
    pub pairable: HashMap<String, (String, String)>,
    /// (serial, path) → contents written via SYNC.
    pub files: HashMap<(String, String), Vec<u8>>,
    /// Every request payload, in order.
    pub requests: Vec<String>,
    next_port: u16,
}

#[derive(Clone)]
pub struct MockAdb {
    pub addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    changed: watch::Sender<u64>,
}

pub struct MockAdbHandle {
    pub mock: MockAdb,
    task: JoinHandle<()>,
}

impl Drop for MockAdbHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl std::ops::Deref for MockAdbHandle {
    type Target = MockAdb;
    fn deref(&self) -> &MockAdb {
        &self.mock
    }
}

impl MockAdb {
    pub async fn start() -> MockAdbHandle {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock adb");
        let addr = listener.local_addr().unwrap();
        let (changed, _) = watch::channel(0);
        let mock = MockAdb {
            addr,
            state: Arc::new(Mutex::new(MockState { next_port: 40000, ..Default::default() })),
            changed,
        };
        let server = mock.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let server = server.clone();
                tokio::spawn(async move {
                    let _ = server.serve(stream).await;
                });
            }
        });
        MockAdbHandle { mock, task }
    }

    pub fn with_state<R>(&self, f: impl FnOnce(&mut MockState) -> R) -> R {
        let r = f(&mut self.state.lock().unwrap());
        self.changed.send_modify(|v| *v += 1);
        r
    }

    pub fn add_device(&self, dev: MockDevice) {
        self.with_state(|s| s.devices.push(dev));
    }

    pub fn set_state(&self, serial: &str, state: &str) {
        self.with_state(|s| {
            if let Some(d) = s.devices.iter_mut().find(|d| d.serial == serial) {
                d.state = state.to_owned();
            }
        });
    }

    pub fn remove_device(&self, serial: &str) {
        self.with_state(|s| s.devices.retain(|d| d.serial != serial));
    }

    pub fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn file(&self, serial: &str, path: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().files.get(&(serial.to_owned(), path.to_owned())).cloned()
    }

    fn listing(&self) -> String {
        let s = self.state.lock().unwrap();
        s.devices
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let usb = d.usb.as_ref().map(|u| format!(" usb:{u}")).unwrap_or_default();
                format!(
                    "{}\t{}{} product:mock model:{} device:mock transport_id:{}\n",
                    d.serial,
                    d.state,
                    usb,
                    d.model.replace(' ', "_"),
                    i + 1
                )
            })
            .collect()
    }

    fn device(&self, serial: &str) -> Option<MockDevice> {
        self.state.lock().unwrap().devices.iter().find(|d| d.serial == serial).cloned()
    }

    async fn serve(&self, mut s: TcpStream) -> std::io::Result<()> {
        let req = read_request(&mut s).await?;
        self.state.lock().unwrap().requests.push(req.clone());

        if req == "host:version" {
            return okay_string(&mut s, "0029").await;
        }
        if req == "host:devices-l" {
            return okay_string(&mut s, &self.listing()).await;
        }
        if req == "host:track-devices-l" {
            s.write_all(b"OKAY").await?;
            let mut rx = self.changed.subscribe();
            loop {
                write_hex_string(&mut s, &self.listing()).await?;
                if rx.changed().await.is_err() {
                    return Ok(());
                }
            }
        }
        if let Some(addr) = req.strip_prefix("host:connect:") {
            let reply = self.with_state(|st| {
                if st.devices.iter().any(|d| d.serial == addr && d.state == "device") {
                    format!("already connected to {addr}")
                } else if let Some(mut dev) = st.connectable.get(addr).cloned() {
                    dev.serial = addr.to_owned();
                    st.devices.retain(|d| d.serial != addr);
                    st.devices.push(dev);
                    format!("connected to {addr}")
                } else {
                    format!("failed to connect to '{addr}': Connection refused")
                }
            });
            return okay_string(&mut s, &reply).await;
        }
        if let Some(addr) = req.strip_prefix("host:disconnect:") {
            self.remove_device(addr);
            return okay_string(&mut s, &format!("disconnected {addr}")).await;
        }
        if let Some(rest) = req.strip_prefix("host:pair:") {
            let (code, addr) = rest.split_once(':').unwrap_or((rest, ""));
            let entry = self.state.lock().unwrap().pairable.get(addr).cloned();
            let failure = match entry {
                Some((expected, guid)) if expected == code => {
                    return okay_string(&mut s, &format!("Successfully paired to {addr} [guid={guid}]")).await;
                }
                Some(_) => "Failed: Wrong password or connection was dropped.",
                None => "Failed: Unable to start pairing client.",
            };
            // Real adb answers failures as OKAY followed by a nested FAIL.
            s.write_all(b"OKAY").await?;
            return fail(&mut s, failure).await;
        }
        if let Some(rest) = req.strip_prefix("host-serial:") {
            return self.host_serial(&mut s, rest).await;
        }
        if let Some(serial) = req.strip_prefix("host:transport:") {
            let Some(dev) = self.device(serial).filter(|d| d.state == "device") else {
                return fail(&mut s, &format!("device '{serial}' not found")).await;
            };
            s.write_all(b"OKAY").await?;
            let service = read_request(&mut s).await?;
            self.state.lock().unwrap().requests.push(service.clone());
            return self.device_service(&mut s, &dev, &service).await;
        }
        fail(&mut s, &format!("unknown host service '{req}'")).await
    }

    async fn host_serial(&self, s: &mut TcpStream, rest: &str) -> std::io::Result<()> {
        // `rest` is "<serial>:<command>"; serials may contain ':' (ip:port),
        // so match the known commands from the right.
        for cmd in ["features", "forward:", "killforward:"] {
            let needle = format!(":{cmd}");
            let Some(idx) = rest.find(&needle) else { continue };
            let serial = &rest[..idx];
            let arg = &rest[idx + needle.len()..];
            let Some(dev) = self.device(serial) else {
                return fail(s, &format!("device '{serial}' not found")).await;
            };
            return match cmd {
                "features" => okay_string(s, &dev.features.join(",")).await,
                "forward:" => {
                    s.write_all(b"OKAYOKAY").await?;
                    if arg.starts_with("tcp:0;") {
                        let port = self.with_state(|st| {
                            st.next_port += 1;
                            st.next_port
                        });
                        write_hex_string(s, &port.to_string()).await?;
                    }
                    Ok(())
                }
                _ => s.write_all(b"OKAYOKAY").await,
            };
        }
        fail(s, "unknown host-serial command").await
    }

    async fn device_service(&self, s: &mut TcpStream, dev: &MockDevice, service: &str) -> std::io::Result<()> {
        if let Some(cmd) = service.strip_prefix("shell,v2,raw:") {
            s.write_all(b"OKAY").await?;
            let (out, code) = run_shell(dev, cmd);
            s.write_all(&v2_packet(1, out.as_bytes())).await?;
            s.write_all(&v2_packet(3, &[code])).await?;
            return Ok(());
        }
        if let Some(cmd) = service.strip_prefix("shell:") {
            s.write_all(b"OKAY").await?;
            s.write_all(run_shell(dev, cmd).0.as_bytes()).await?;
            return Ok(());
        }
        if let Some(port) = service.strip_prefix("tcpip:") {
            s.write_all(b"OKAY").await?;
            return s.write_all(format!("restarting in TCP mode port: {port}\n").as_bytes()).await;
        }
        if service == "sync:" {
            s.write_all(b"OKAY").await?;
            return self.sync(s, &dev.serial).await;
        }
        fail(s, &format!("unknown device service '{service}'")).await
    }

    async fn sync(&self, s: &mut TcpStream, serial: &str) -> std::io::Result<()> {
        loop {
            let mut hdr = [0u8; 8];
            if s.read_exact(&mut hdr).await.is_err() {
                return Ok(());
            }
            let len = u32::from_le_bytes(hdr[4..].try_into().unwrap()) as usize;
            match &hdr[..4] {
                b"STAT" => {
                    let path = read_exact_string(s, len).await?;
                    let size = self.file(serial, &path).map(|f| f.len() as u32);
                    let mut resp = b"STAT".to_vec();
                    let (mode, size) = size.map_or((0, 0), |sz| (0o100644u32, sz));
                    resp.extend(mode.to_le_bytes());
                    resp.extend(size.to_le_bytes());
                    resp.extend(0u32.to_le_bytes());
                    s.write_all(&resp).await?;
                }
                b"SEND" => {
                    let spec = read_exact_string(s, len).await?;
                    let path = spec.rsplit_once(',').map_or(spec.as_str(), |(p, _)| p).to_owned();
                    let mut data = Vec::new();
                    loop {
                        let mut h = [0u8; 8];
                        s.read_exact(&mut h).await?;
                        let n = u32::from_le_bytes(h[4..].try_into().unwrap()) as usize;
                        match &h[..4] {
                            b"DATA" => {
                                let mut chunk = vec![0u8; n];
                                s.read_exact(&mut chunk).await?;
                                data.extend(chunk);
                            }
                            b"DONE" => break,
                            _ => return Ok(()),
                        }
                    }
                    self.with_state(|st| st.files.insert((serial.to_owned(), path), data));
                    s.write_all(b"OKAY\0\0\0\0").await?;
                }
                _ => return Ok(()), // QUIT or garbage
            }
        }
    }
}

fn run_shell(dev: &MockDevice, cmd: &str) -> (String, u8) {
    if let Some((out, code)) = dev.shell.get(cmd) {
        return (out.clone(), *code);
    }
    if let Some(name) = cmd.strip_prefix("getprop ") {
        let v = dev.props.get(name.trim()).cloned().unwrap_or_default();
        return (format!("{v}\n"), 0);
    }
    if let Some(text) = cmd.strip_prefix("echo ") {
        return (format!("{text}\n"), 0);
    }
    (String::new(), 127)
}

fn v2_packet(id: u8, data: &[u8]) -> Vec<u8> {
    let mut v = vec![id];
    v.extend((data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    v
}

async fn read_request(s: &mut TcpStream) -> std::io::Result<String> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await?;
    let len = usize::from_str_radix(std::str::from_utf8(&len).unwrap_or("0"), 16).unwrap_or(0);
    read_exact_string(s, len).await
}

async fn read_exact_string(s: &mut TcpStream, len: usize) -> std::io::Result<String> {
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn write_hex_string(s: &mut TcpStream, text: &str) -> std::io::Result<()> {
    s.write_all(format!("{:04x}{text}", text.len()).as_bytes()).await
}

async fn okay_string(s: &mut TcpStream, text: &str) -> std::io::Result<()> {
    s.write_all(b"OKAY").await?;
    write_hex_string(s, text).await
}

async fn fail(s: &mut TcpStream, msg: &str) -> std::io::Result<()> {
    s.write_all(b"FAIL").await?;
    write_hex_string(s, msg).await
}
