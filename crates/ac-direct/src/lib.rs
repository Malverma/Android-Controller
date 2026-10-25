//! Client for the Android helper's PC connection without ADB ("direct
//! mode", android-helper/src/org/androidcontroller/keeper/DirectService.java).
//!
//! The helper listens on [`PORT`] and announces `_acdirect._tcp`. Every
//! connection starts with `ACD1` and a [`Purpose`] byte; control messages
//! are JSON objects framed by a big-endian `u32` length.
//!
//! - **Pairing:** P-256 ECDH, confirmed with the 6-digit code the phone
//!   shows. Both sides keep `key = HMAC(shared, "acd-key" ‖ transcript ‖
//!   SHA-256(code))`. A passive listener learns nothing; an active attacker
//!   on the LAN at the moment of pairing could still guess the code offline,
//!   which is why the phone opens pairing only on request, for two minutes.
//! - **Every later connection:** a mutual HMAC challenge with that key. The
//!   streams themselves are not encrypted (like `adb tcpip`).
//! - **Stream:** after the handshake the socket carries scrcpy's video
//!   format to the PC and scrcpy control messages to the phone.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use p256::{PublicKey, SecretKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub const PORT: u16 = 27190;
pub const SERVICE_TYPE: &str = "_acdirect._tcp";
const MAGIC: &[u8; 4] = b"ACD1";
const MAX_JSON: u32 = 1 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// The phone may wait for the user to allow screen capture.
const STREAM_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Purpose {
    Pair = 1,
    Link = 2,
    Stream = 3,
    Push = 4,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    /// The phone said no; `message` is meant for the user.
    #[error("{message}")]
    Refused { message: String, unpaired: bool, need_capture: bool },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("a different phone ({found}) answers at this address")]
    WrongDevice { found: String },
    #[error("the phone did not answer in time")]
    Timeout,
}

impl Error {
    fn protocol(m: impl Into<String>) -> Self {
        Self::Protocol(m.into())
    }

    /// The phone no longer knows this PC (forgotten, or reinstalled).
    pub fn is_unpaired(&self) -> bool {
        matches!(self, Self::Refused { unpaired: true, .. })
    }
}

// ---- framing ------------------------------------------------------------------------

/// Reads one JSON message; an `{"error": …}` reply becomes [`Error::Refused`].
pub async fn read_json<R: AsyncRead + Unpin>(r: &mut R) -> Result<Value, Error> {
    let n = r.read_u32().await?;
    if !(2..=MAX_JSON).contains(&n) {
        return Err(Error::protocol(format!("bad message length {n}")));
    }
    let mut b = vec![0u8; n as usize];
    r.read_exact(&mut b).await?;
    let v: Value = serde_json::from_slice(&b).map_err(|e| Error::protocol(e.to_string()))?;
    if let Some(message) = v.get("error").and_then(Value::as_str) {
        return Err(Error::Refused {
            message: message.to_owned(),
            unpaired: v.get("unpaired").and_then(Value::as_bool).unwrap_or(false),
            need_capture: v.get("need_capture").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    Ok(v)
}

pub async fn write_json<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> Result<(), Error> {
    let b = serde_json::to_vec(v).map_err(|e| Error::protocol(e.to_string()))?;
    let mut msg = Vec::with_capacity(4 + b.len());
    msg.extend_from_slice(&(b.len() as u32).to_be_bytes());
    msg.extend_from_slice(&b);
    w.write_all(&msg).await?;
    w.flush().await?;
    Ok(())
}

// ---- crypto helpers -------------------------------------------------------------------

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// Constant-time comparison.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rng().fill_bytes(&mut b);
    b
}

fn random_secret() -> SecretKey {
    loop {
        // Out-of-range scalars are astronomically rare; just draw again.
        if let Ok(k) = SecretKey::from_slice(&random_bytes::<32>()) {
            return k;
        }
    }
}

fn str_field<'a>(v: &'a Value, k: &str) -> Result<&'a str, Error> {
    v.get(k).and_then(Value::as_str).ok_or_else(|| Error::protocol(format!("reply has no {k}")))
}

fn hex_field(v: &Value, k: &str) -> Result<Vec<u8>, Error> {
    unhex(str_field(v, k)?).ok_or_else(|| Error::protocol(format!("{k} is not hex")))
}

// ---- keys ---------------------------------------------------------------------------------

/// This PC's id and the keys of the phones paired with it
/// (`direct-keys.toml`, readable by the user only).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyStore {
    pub pc_id: String,
    #[serde(default, rename = "device")]
    pub devices: Vec<PairedDevice>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedDevice {
    pub id: String,
    #[serde(default)]
    pub model: String,
    /// Hex of the 32-byte key.
    pub key: String,
}

/// What a connection task needs: who we are and the keys (a snapshot).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub pc_id: String,
    pub pc_name: String,
    pub keys: BTreeMap<String, [u8; 32]>,
}

impl KeyStore {
    /// Loads `path`, or starts a new store (with a new PC id) there.
    pub fn load(path: &Path) -> Self {
        let mut ks = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<KeyStore>(&text).unwrap_or_else(|e| {
                tracing::warn!("{}: {e}; starting a new key store", path.display());
                KeyStore::default()
            }),
            Err(_) => KeyStore::default(),
        };
        ks.path = Some(path.to_owned());
        if ks.pc_id.is_empty() {
            ks.pc_id = hex(&random_bytes::<16>());
            if let Err(e) = ks.save() {
                tracing::warn!("saving {}: {e}", path.display());
            }
        }
        ks
    }

    pub fn in_memory() -> Self {
        Self { pc_id: hex(&random_bytes::<16>()), ..Self::default() }
    }

    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        {
            use std::io::Write;
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
            let mut f = o.open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    pub fn key(&self, id: &str) -> Option<[u8; 32]> {
        let d = self.devices.iter().find(|d| d.id == id)?;
        unhex(&d.key)?.try_into().ok()
    }

    pub fn is_paired(&self, id: &str) -> bool {
        self.key(id).is_some()
    }

    pub fn insert(&mut self, paired: &Paired) {
        self.devices.retain(|d| d.id != paired.id);
        self.devices.push(PairedDevice { id: paired.id.clone(), model: paired.model.clone(), key: hex(&paired.key) });
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let n = self.devices.len();
        self.devices.retain(|d| d.id != id);
        n != self.devices.len()
    }

    pub fn credentials(&self) -> Credentials {
        Credentials {
            pc_id: self.pc_id.clone(),
            pc_name: pc_name(),
            keys: self.devices.iter().filter_map(|d| Some((d.id.clone(), self.key(&d.id)?))).collect(),
        }
    }
}

/// The name the phone shows for this PC.
pub fn pc_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "PC".into())
}

// ---- connecting -----------------------------------------------------------------------------

async fn dial(addr: SocketAddr, purpose: Purpose) -> Result<TcpStream, Error> {
    let s = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await.map_err(|_| Error::Timeout)??;
    s.set_nodelay(true)?;
    let mut s = s;
    let mut hello = MAGIC.to_vec();
    hello.push(purpose as u8);
    s.write_all(&hello).await?;
    Ok(s)
}

/// Result of a pairing: the phone and the key now shared with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    pub id: String,
    pub model: String,
    pub key: [u8; 32],
}

/// Pairs with the phone at `addr` using the code it shows.
pub async fn pair(addr: SocketAddr, pc_id: &str, pc_name: &str, code: &str) -> Result<Paired, Error> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, pair_inner(addr, pc_id, pc_name, code)).await.map_err(|_| Error::Timeout)?
}

async fn pair_inner(addr: SocketAddr, pc_id: &str, pc_name: &str, code: &str) -> Result<Paired, Error> {
    let mut s = dial(addr, Purpose::Pair).await?;
    let secret = random_secret();
    let ours = secret.public_key().to_encoded_point(false);
    let ours = ours.as_bytes();
    write_json(&mut s, &json!({ "pc_id": pc_id, "pc_name": pc_name, "pub": hex(ours) })).await?;
    let reply = read_json(&mut s).await?;
    let theirs_bytes = hex_field(&reply, "pub")?;
    let theirs = PublicKey::from_sec1_bytes(&theirs_bytes).map_err(|_| Error::protocol("bad public key"))?;
    let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), theirs.as_affine());
    let shared = shared.raw_secret_bytes();
    let transcript = sha256(&[ours, &theirs_bytes]);
    let code_key = sha256(&[format!("acd-code:{code}").as_bytes()]);
    let confirm = hmac(&code_key, &[b"pc", &transcript, shared]);
    write_json(&mut s, &json!({ "confirm": hex(&confirm) })).await?;
    let ok = read_json(&mut s).await?;
    let want = hmac(&code_key, &[b"phone", &transcript, shared]);
    if !same(&hex_field(&ok, "confirm")?, &want) {
        return Err(Error::protocol("the phone's pairing confirmation did not match"));
    }
    Ok(Paired {
        id: str_field(&reply, "id")?.to_owned(),
        model: reply.get("model").and_then(Value::as_str).unwrap_or_default().to_owned(),
        key: hmac(shared, &[b"acd-key", &transcript, &code_key]),
    })
}

/// An authenticated connection and the phone's answer to its request.
pub struct Session {
    pub stream: TcpStream,
    pub device_id: String,
    pub reply: Value,
}

/// Opens an authenticated connection for `purpose`, sending `params` (a JSON
/// object) as the request. `expect` guards against another phone answering
/// at a remembered address.
pub async fn open(addr: SocketAddr, creds: &Credentials, expect: Option<&str>, purpose: Purpose, params: Value) -> Result<Session, Error> {
    let limit = if purpose == Purpose::Stream { STREAM_HANDSHAKE_TIMEOUT } else { HANDSHAKE_TIMEOUT };
    tokio::time::timeout(limit, open_inner(addr, creds, expect, purpose, params)).await.map_err(|_| Error::Timeout)?
}

async fn open_inner(addr: SocketAddr, creds: &Credentials, expect: Option<&str>, purpose: Purpose, params: Value) -> Result<Session, Error> {
    let mut s = dial(addr, purpose).await?;
    let nc = random_bytes::<16>();
    write_json(&mut s, &json!({ "pc_id": creds.pc_id, "nonce": hex(&nc) })).await?;
    let hello = read_json(&mut s).await?;
    let id = str_field(&hello, "id")?.to_owned();
    if expect.is_some_and(|e| e != id) {
        return Err(Error::WrongDevice { found: id });
    }
    let Some(key) = creds.keys.get(&id) else {
        return Err(Error::Refused { message: "This PC has no key for that phone; pair it again".into(), unpaired: true, need_capture: false });
    };
    let ns = hex_field(&hello, "nonce")?;
    let p = [purpose as u8];
    if !same(&hex_field(&hello, "proof")?, &hmac(key, &[b"S", &nc, &ns, &p])) {
        return Err(Error::protocol("the phone failed authentication (paired with another PC under this id?)"));
    }
    let mut req: Map<String, Value> = match params {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    req.insert("proof".into(), Value::String(hex(&hmac(key, &[b"C", &nc, &ns, &p]))));
    write_json(&mut s, &Value::Object(req)).await?;
    let reply = read_json(&mut s).await?;
    Ok(Session { stream: s, device_id: id, reply })
}

// ---- link -------------------------------------------------------------------------------------

/// The phone as it introduces itself on a link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub id: String,
    pub model: String,
    pub manufacturer: String,
    pub sdk: u32,
    pub release: String,
    /// Touch control (the accessibility service) is on.
    pub input: bool,
    /// Screen capture is already allowed.
    pub capture: bool,
    /// The helper's version code (0 before 0.6, which did not send it).
    pub version: u32,
    /// Video codecs the helper can encode (`["h264"]` before 0.6).
    pub codecs: Vec<String>,
}

/// The helper version that knows app actions, multi-touch, the phone's
/// clipboard and performance stats.
pub const HELPER_V6: u32 = 6;

/// Telemetry in a heartbeat answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pong {
    pub battery: Option<u8>,
    pub charging: bool,
    pub rssi: Option<i16>,
    pub link_mbps: Option<u32>,
    pub input: bool,
    pub capture: bool,
    /// Helper 0.6+: performance stats (see `ac-device` perf).
    pub cpu_pct: Option<f32>,
    pub mem_used_mb: Option<u32>,
    pub mem_total_mb: Option<u32>,
    pub battery_temp_c: Option<f32>,
    /// Helper 0.6+: the phone's clipboard, when it changed (`clip_seq`
    /// counts changes so the same text copied twice still arrives).
    pub clip: Option<String>,
    pub clip_seq: u64,
}

/// The long-lived connection: heartbeat, telemetry, "keep screen on".
#[derive(Debug)]
pub struct Link {
    stream: TcpStream,
    pub hello: Hello,
}

impl Link {
    pub async fn connect(addr: SocketAddr, creds: &Credentials, expect: Option<&str>) -> Result<Link, Error> {
        let Session { stream, device_id, reply } = open(addr, creds, expect, Purpose::Link, json!({})).await?;
        let s = |k: &str| reply.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        let b = |k: &str| reply.get(k).and_then(Value::as_bool).unwrap_or(false);
        let hello = Hello {
            id: device_id,
            model: s("model"),
            manufacturer: s("manufacturer"),
            sdk: reply.get("sdk").and_then(Value::as_u64).unwrap_or(0) as u32,
            release: s("release"),
            input: b("input"),
            capture: b("capture"),
            version: reply.get("version").and_then(Value::as_u64).unwrap_or(0) as u32,
            codecs: match reply.get("codecs").and_then(Value::as_array) {
                Some(a) => a.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
                None => vec!["h264".to_owned()],
            },
        };
        Ok(Link { stream, hello })
    }

    async fn request(&mut self, v: Value, limit: Duration) -> Result<Value, Error> {
        tokio::time::timeout(limit, async {
            write_json(&mut self.stream, &v).await?;
            read_json(&mut self.stream).await
        })
        .await
        .map_err(|_| Error::Timeout)?
    }

    /// Heartbeat; returns the phone's telemetry and the round trip.
    pub async fn ping(&mut self, limit: Duration) -> Result<(Pong, Duration), Error> {
        let start = Instant::now();
        let v = self.request(json!({ "op": "ping" }), limit).await?;
        let rtt = start.elapsed();
        Ok((
            Pong {
                battery: v.get("battery").and_then(Value::as_u64).map(|b| b.min(100) as u8),
                charging: v.get("charging").and_then(Value::as_bool).unwrap_or(false),
                rssi: v.get("rssi").and_then(Value::as_i64).map(|r| r as i16),
                link_mbps: v.get("link_mbps").and_then(Value::as_u64).map(|m| m as u32),
                input: v.get("input").and_then(Value::as_bool).unwrap_or(false),
                capture: v.get("capture").and_then(Value::as_bool).unwrap_or(false),
                cpu_pct: v.get("cpu").and_then(Value::as_f64).map(|c| c as f32),
                mem_used_mb: v.get("mem_used_mb").and_then(Value::as_u64).map(|m| m as u32),
                mem_total_mb: v.get("mem_total_mb").and_then(Value::as_u64).map(|m| m as u32),
                battery_temp_c: v.get("battery_temp").and_then(Value::as_f64).map(|t| t as f32),
                clip: v.get("clip").and_then(Value::as_str).map(str::to_owned),
                clip_seq: v.get("clip_seq").and_then(Value::as_u64).unwrap_or(0),
            },
            rtt,
        ))
    }

    /// Reads (`None`) or sets "keep screen on"; returns the state after.
    pub async fn stay_awake(&mut self, on: Option<bool>, limit: Duration) -> Result<bool, Error> {
        let v = self.request(json!({ "op": "stay_awake", "on": on }), limit).await?;
        Ok(v.get("on").and_then(Value::as_bool).unwrap_or(false))
    }

    /// Helper 0.6+: `action` is `launch`, `stop`, `clear` or `uninstall`.
    /// Uninstalling asks on the phone (Android lets no app do it silently).
    /// `Ok(Err(reason))` when the phone said no.
    pub async fn app(&mut self, action: &str, package: &str, limit: Duration) -> Result<Result<String, String>, Error> {
        let v = self.request(json!({ "op": "app", "action": action, "package": package }), limit).await?;
        let msg = v.get("message").and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok(if v.get("ok").and_then(Value::as_bool).unwrap_or(false) { Ok(msg) } else { Err(msg) })
    }

    /// Helper 0.6+: the apps the user installed (package names).
    pub async fn packages(&mut self, limit: Duration) -> Result<Vec<String>, Error> {
        let v = self.request(json!({ "op": "packages" }), limit).await?;
        let mut list: Vec<String> =
            v.get("packages").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        list.sort();
        Ok(list)
    }
}

// ---- stream and push ------------------------------------------------------------------------

/// Mirroring parameters sent with a STREAM request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamParams {
    pub max_size: u32,
    pub bit_rate: u32,
    pub max_fps: u32,
    pub power_on: bool,
    /// `"h264"` or `"h265"`; helpers before 0.6 always send H.264.
    pub codec: &'static str,
}

/// Opens a mirroring session. Returns the socket (positioned at the codec
/// id of the video stream) and whether touch control is on.
pub async fn open_stream(addr: SocketAddr, creds: &Credentials, expect: &str, p: StreamParams) -> Result<(TcpStream, bool), Error> {
    let params = json!({ "max_size": p.max_size, "bit_rate": p.bit_rate, "max_fps": p.max_fps, "power_on": p.power_on, "codec": p.codec });
    let s = open(addr, creds, Some(expect), Purpose::Stream, params).await?;
    let input = s.reply.get("input").and_then(Value::as_bool).unwrap_or(false);
    Ok((s.stream, input))
}

/// One file for the phone's Download folder (or, with `install`, an APK
/// for its installer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushRequest {
    pub name: String,
    /// Folder below Download, `/`-separated; empty for Download itself.
    pub dir: String,
    pub size: u64,
    /// Identifies a partial upload to resume.
    pub job: u64,
    pub install: bool,
    /// `rename`, `overwrite` or `skip`.
    pub conflict: String,
}

pub enum PushStart {
    /// `skip` and the file exists.
    Skip,
    /// Send the bytes from this offset on.
    From(u64),
}

pub async fn open_push(addr: SocketAddr, creds: &Credentials, expect: &str, r: &PushRequest) -> Result<(TcpStream, PushStart), Error> {
    let params = json!({
        "name": r.name, "dir": r.dir, "size": r.size, "job": format!("{:016x}", r.job),
        "install": r.install, "conflict": r.conflict,
    });
    let s = open(addr, creds, Some(expect), Purpose::Push, params).await?;
    if s.reply.get("skip").and_then(Value::as_bool).unwrap_or(false) {
        return Ok((s.stream, PushStart::Skip));
    }
    let offset = s.reply.get("offset").and_then(Value::as_u64).unwrap_or(0).min(r.size);
    Ok((s.stream, PushStart::From(offset)))
}

/// What the phone did with a pushed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushDone {
    pub path: Option<String>,
    pub sha256: Option<String>,
    pub installed: bool,
}

/// Waits for the phone's answer after the last byte (an install waits for
/// the user to confirm on the phone).
pub async fn finish_push(stream: &mut TcpStream, limit: Duration) -> Result<PushDone, Error> {
    let v = tokio::time::timeout(limit, read_json(stream)).await.map_err(|_| Error::Timeout)??;
    Ok(PushDone {
        path: v.get("path").and_then(Value::as_str).map(str::to_owned),
        sha256: v.get("sha256").and_then(Value::as_str).map(str::to_owned),
        installed: v.get("installed").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// `ip`, `ip:port` → the helper's address (port [`PORT`] by default).
pub fn parse_addr(input: &str) -> Option<SocketAddr> {
    let s = input.trim();
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Some(a);
    }
    s.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok().map(|ip| SocketAddr::new(ip, PORT))
}

#[cfg(test)]
mod tests;
