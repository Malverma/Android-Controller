//! Focus-view audio (spec §4.4 step 5). scrcpy-server captures the phone's
//! output (Android 11+) and sends it as raw PCM: 48 kHz, stereo, s16le, in
//! the same 12-byte-header packets as video. `pw-cat` plays it as a
//! PipeWire stream.
//!
//! A reader thread drains the socket into a short queue and a writer thread
//! feeds the player. When playback falls behind (Wi-Fi bursts, the two
//! clocks drifting apart), the oldest audio is dropped so the delay stays
//! under [`MAX_QUEUED`] instead of growing without bound.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};

/// scrcpy's `raw` audio codec id.
pub const CODEC_RAW: u32 = 0x0072_6177;
const RATE: u32 = 48_000;
const BYTES_PER_FRAME: usize = 4;
/// Queued audio beyond this (200 ms) is dropped from the front.
pub const MAX_QUEUED: usize = RATE as usize * BYTES_PER_FRAME / 5;
const PACKET_MAX: usize = 1 << 20;
const FLAG_CONFIG: u64 = 1 << 62;
/// Keeps the player's input pipe from hiding a large backlog.
const PIPE_SIZE: libc::c_int = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the phone could not capture its audio")]
    DeviceFailed,
    #[error("the phone sent audio in an unexpected format ({0:#010x})")]
    Codec(u32),
    #[error("can't start {binary}: {source} (it comes with PipeWire: install pipewire-audio)")]
    Player { binary: String, source: std::io::Error },
    #[error("audio stream: {0}")]
    Io(#[from] std::io::Error),
    #[error("audio stream: bad packet of {0} bytes")]
    Packet(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// PipeWire `node.name`, e.g. `android-controller.slot2`.
    pub node_name: String,
    /// Shown in volume mixers: the phone's name.
    pub description: String,
    /// 0.0..=1.0.
    pub volume: f64,
    /// Player buffer in frames at 48 kHz.
    pub latency_frames: u32,
    pub binary: PathBuf,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            node_name: "android-controller".into(),
            description: "Android phone".into(),
            volume: 1.0,
            latency_frames: 2048,
            binary: "pw-cat".into(),
        }
    }
}

impl Options {
    /// The `pw-cat` command line: raw s16 stereo from stdin.
    pub fn command(&self) -> Command {
        let mut c = Command::new(&self.binary);
        c.args(["--playback", "--raw", "--format", "s16", "--rate", "48000", "--channels", "2", "--media-role", "Game"])
            .arg("--latency")
            .arg(self.latency_frames.to_string())
            .arg("--volume")
            .arg(format!("{:.3}", self.volume.clamp(0.0, 1.0)))
            .arg("--properties")
            .arg(format!(
                "{{ node.name = \"{}\" node.description = \"{}\" application.name = \"Android Controller\" }}",
                sanitize(&self.node_name),
                sanitize(&self.description)
            ))
            .arg("-");
        c
    }
}

/// SPA-JSON string contents: no quotes or control characters.
fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control() && *c != '"' && *c != '\\').collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The phone can't capture audio (Android 10 or older); nothing played.
    Unsupported,
    /// The stream closed (the session stopped).
    Closed,
}

/// Plays `socket`'s audio until it closes. Blocking: run it on its own
/// thread, and shut the socket down to stop it.
pub fn run(mut socket: TcpStream, opts: &Options, player: Command) -> Result<Ended, Error> {
    let mut id = [0u8; 4];
    socket.read_exact(&mut id)?;
    match u32::from_be_bytes(id) {
        0 => return Ok(Ended::Unsupported),
        1 => return Err(Error::DeviceFailed),
        CODEC_RAW => {}
        other => return Err(Error::Codec(other)),
    }
    let child = spawn_player(player, opts)?;
    let queue = Arc::new(Queue::default());
    let writer = {
        let queue = queue.clone();
        std::thread::Builder::new().name("audio-out".into()).spawn(move || feed(child, &queue))?
    };
    let r = read_packets(&mut socket, &queue);
    queue.close();
    let _ = writer.join();
    let dropped = queue.dropped();
    if dropped > 0 {
        tracing::debug!("audio: dropped {} ms to stay in sync", dropped / (RATE as usize * BYTES_PER_FRAME / 1000));
    }
    match r {
        Ok(()) => Ok(Ended::Closed),
        // A shutdown from the session looks like a reset or EOF.
        Err(Error::Io(e)) if is_closed(&e) => Ok(Ended::Closed),
        Err(e) => Err(e),
    }
}

fn is_closed(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), UnexpectedEof | ConnectionReset | ConnectionAborted | BrokenPipe | NotConnected)
}

fn spawn_player(mut cmd: Command, opts: &Options) -> Result<Child, Error> {
    let child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| Error::Player { binary: opts.binary.display().to_string(), source })?;
    if let Some(stdin) = &child.stdin {
        use std::os::fd::AsRawFd;
        // SAFETY: a valid pipe fd owned by `child` for this call.
        unsafe { libc::fcntl(stdin.as_raw_fd(), libc::F_SETPIPE_SZ, PIPE_SIZE) };
    }
    Ok(child)
}

/// Reads media packets into `queue` until the socket ends.
fn read_packets(socket: &mut impl Read, queue: &Queue) -> Result<(), Error> {
    let mut header = [0u8; 12];
    loop {
        match socket.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let flags = u64::from_be_bytes(header[..8].try_into().unwrap());
        let len = u32::from_be_bytes(header[8..].try_into().unwrap());
        if len as usize > PACKET_MAX {
            return Err(Error::Packet(len));
        }
        let mut data = vec![0u8; len as usize];
        socket.read_exact(&mut data)?;
        if flags & FLAG_CONFIG == 0 && !data.is_empty() {
            queue.push(data);
        }
    }
}

/// Writes queued audio to the player until the queue closes or it exits,
/// then ends its input and gives it a moment to play out.
fn feed(mut child: Child, queue: &Queue) {
    if let Some(mut stdin) = child.stdin.take() {
        while let Some(chunk) = queue.pop() {
            if stdin.write_all(&chunk).is_err() {
                tracing::warn!("audio player stopped");
                break;
            }
        }
    }
    for _ in 0..20 {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[derive(Default)]
struct QueueState {
    chunks: VecDeque<Vec<u8>>,
    bytes: usize,
    dropped: usize,
    closed: bool,
}

#[derive(Default)]
struct Queue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

impl Queue {
    fn push(&self, chunk: Vec<u8>) {
        let mut s = self.state.lock().unwrap();
        s.bytes += chunk.len();
        s.chunks.push_back(chunk);
        while s.bytes > MAX_QUEUED && s.chunks.len() > 1 {
            let old = s.chunks.pop_front().expect("non-empty");
            s.bytes -= old.len();
            s.dropped += old.len();
        }
        self.ready.notify_one();
    }

    /// The next chunk; `None` once closed and drained.
    fn pop(&self) -> Option<Vec<u8>> {
        let mut s = self.state.lock().unwrap();
        loop {
            if let Some(c) = s.chunks.pop_front() {
                s.bytes -= c.len();
                return Some(c);
            }
            if s.closed {
                return None;
            }
            s = self.ready.wait(s).unwrap();
        }
    }

    fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_all();
    }

    fn dropped(&self) -> usize {
        self.state.lock().unwrap().dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn packet(flags: u64, data: &[u8]) -> Vec<u8> {
        let mut p = flags.to_be_bytes().to_vec();
        p.extend((data.len() as u32).to_be_bytes());
        p.extend(data);
        p
    }

    #[test]
    fn queue_drops_the_oldest_audio_past_the_limit() {
        let q = Queue::default();
        let chunk = MAX_QUEUED / 4;
        for i in 0..6u8 {
            q.push(vec![i; chunk]);
        }
        assert_eq!(q.dropped(), 2 * chunk);
        assert_eq!(q.pop().unwrap()[0], 2, "the newest 200 ms are kept");
        q.close();
        assert_eq!(std::iter::from_fn(|| q.pop()).count(), 3, "closing drains what is left");
    }

    #[test]
    fn config_packets_are_skipped() {
        let mut wire = packet(FLAG_CONFIG, b"cfg");
        wire.extend(packet(1000, b"pcm1"));
        wire.extend(packet(2000, b""));
        wire.extend(packet(3000, b"pcm2"));
        let q = Queue::default();
        read_packets(&mut wire.as_slice(), &q).unwrap();
        assert_eq!((q.pop().unwrap(), q.pop().unwrap()), (b"pcm1".to_vec(), b"pcm2".to_vec()));
        // Ending between packets is a clean close; inside one is not.
        assert!(read_packets(&mut &packet(0, &[]).as_slice()[..10], &q).is_ok());
        assert!(matches!(read_packets(&mut &packet(0, b"abcd").as_slice()[..14], &q), Err(Error::Io(_))));
    }

    #[test]
    fn command_line() {
        let o = Options { node_name: "android-controller.slot2".into(), description: "Moto \"g\"".into(), volume: 1.7, ..Options::default() };
        let c = o.command();
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(c.get_program(), "pw-cat");
        assert!(args.windows(2).any(|w| w == ["--volume", "1.000"]), "{args:?}");
        assert!(args.windows(2).any(|w| w == ["--latency", "2048"]));
        assert!(args.iter().any(|a| a.contains("node.description = \"Moto g\"")), "{args:?}");
        assert_eq!(args.last().unwrap(), "-");
    }

    /// End to end through a real socket, with `cat` standing in for pw-cat.
    #[test]
    fn plays_until_the_socket_closes() {
        let dir = std::env::temp_dir().join(format!("ac-audio-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("pcm");
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let phone = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(&CODEC_RAW.to_be_bytes()).unwrap();
            s.write_all(&packet(0, &[1, 2, 3, 4])).unwrap();
            s.write_all(&packet(10, &[5, 6, 7, 8])).unwrap();
        });
        let socket = TcpStream::connect(addr).unwrap();
        let mut cat = Command::new("sh");
        cat.arg("-c").arg(format!("cat > '{}'", out.display()));
        let ended = run(socket, &Options::default(), cat).unwrap();
        phone.join().unwrap();
        assert_eq!(ended, Ended::Closed);
        assert_eq!(std::fs::read(&out).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_phone_without_audio_capture_is_not_an_error() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let phone = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            s.write_all(&0u32.to_be_bytes()).unwrap();
        });
        let r = run(TcpStream::connect(addr).unwrap(), &Options::default(), Command::new("false"));
        phone.join().unwrap();
        assert_eq!(r.unwrap(), Ended::Unsupported);
    }
}
