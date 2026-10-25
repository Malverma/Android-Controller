//! `scrcpy-server` version coupling and launch command (spec §1.4, §4.2).

use crate::demux::VideoCodec;

/// Where the server jar is pushed. The server unlinks itself as soon as it
/// starts (`CleanUp.unlinkSelf`), so it is pushed before every launch.
pub const DEVICE_JAR: &str = "/data/local/tmp/android-controller-server.jar";

/// Parses the first line of `scrcpy --version`: `scrcpy 4.1 <https://…>`.
pub fn parse_version(output: &str) -> Option<String> {
    let line = output.lines().next()?;
    let v = line.strip_prefix("scrcpy ")?.split_whitespace().next()?;
    let ok = !v.is_empty() && v.split('.').all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    (ok && v.contains('.')).then(|| v.to_owned())
}

/// This client implements the 4.x wire protocol.
pub fn is_compatible(version: &str) -> bool {
    version.split('.').next() == Some("4")
}

/// Runs `scrcpy --version` (blocking; call from `spawn_blocking`).
pub fn detect_version(binary: &std::path::Path) -> Result<String, String> {
    let out = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| format!("cannot run {}: {e}", binary.display()))?;
    parse_version(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| format!("unexpected `{} --version` output", binary.display()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServerParams {
    /// 31-bit session id; names the abstract socket `scrcpy_<scid:08x>`.
    pub scid: u32,
    pub codec: VideoCodec,
    pub bit_rate: u32,
    /// Longest side in pixels; 0 = native.
    pub max_size: u32,
    pub max_fps: u32,
    pub codec_options: String,
    pub control: bool,
    pub clipboard_autosync: bool,
    pub stay_awake: bool,
    pub show_touches: bool,
    pub power_on: bool,
    /// Also capture the phone's audio (Android 11+), as raw 48 kHz s16 PCM.
    pub audio: bool,
}

impl Default for ServerParams {
    fn default() -> Self {
        Self {
            scid: 0,
            codec: VideoCodec::H264,
            bit_rate: 3_000_000,
            max_size: 1280,
            max_fps: 30,
            codec_options: "i-frame-interval:int=10".into(),
            control: true,
            clipboard_autosync: true,
            stay_awake: false,
            show_touches: false,
            power_on: true,
            audio: false,
        }
    }
}

impl ServerParams {
    pub fn socket_name(&self) -> String {
        format!("scrcpy_{:08x}", self.scid)
    }

    /// The server's `key=value` arguments (after the version).
    pub fn args(&self) -> Vec<String> {
        let mut a = vec![
            format!("scid={:08x}", self.scid),
            "log_level=info".into(),
            "tunnel_forward=true".into(),
            "video=true".into(),
            format!("audio={}", self.audio),
            format!("video_codec={}", self.codec.option()),
            format!("video_bit_rate={}", self.bit_rate),
            format!("max_size={}", self.max_size),
        ];
        if self.max_fps > 0 {
            a.push(format!("max_fps={}", self.max_fps));
        }
        if !self.codec_options.is_empty() {
            a.push(format!("video_codec_options={}", self.codec_options));
        }
        if self.audio {
            // Uncompressed: nothing to decode, and 1.5 Mbit/s is little on a LAN.
            a.push("audio_codec=raw".into());
        }
        a.extend([
            format!("control={}", self.control),
            format!("clipboard_autosync={}", self.clipboard_autosync),
            "send_device_meta=true".into(),
            "send_frame_meta=true".into(),
            "send_dummy_byte=true".into(),
            "send_stream_meta=true".into(),
            "cleanup=true".into(),
            format!("power_on={}", self.power_on),
            "power_off_on_close=false".into(),
            "downsize_on_error=true".into(),
            format!("stay_awake={}", self.stay_awake),
            format!("show_touches={}", self.show_touches),
        ]);
        a
    }

    /// The full shell command line, each argument single-quoted.
    pub fn command(&self, version: &str) -> String {
        let mut cmd = format!("CLASSPATH={DEVICE_JAR} app_process / com.genymobile.scrcpy.Server {}", quote(version));
        for arg in self.args() {
            cmd.push(' ');
            cmd.push_str(&quote(&arg));
        }
        cmd
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing() {
        assert_eq!(parse_version("scrcpy 4.1 <https://github.com/Genymobile/scrcpy>\n\nDependencies").as_deref(), Some("4.1"));
        assert_eq!(parse_version("scrcpy 4.0.2 <x>").as_deref(), Some("4.0.2"));
        assert_eq!(parse_version("scrcpy v4 <x>"), None);
        assert_eq!(parse_version("hello"), None);
        assert!(is_compatible("4.1"));
        assert!(!is_compatible("3.3.4"));
        assert!(!is_compatible("40.1"));
    }

    #[test]
    fn command_line() {
        let p = ServerParams { scid: 0xabcd, codec_options: "a:int=1'b".into(), ..Default::default() };
        let cmd = p.command("4.1");
        assert!(cmd.starts_with(
            "CLASSPATH=/data/local/tmp/android-controller-server.jar app_process / com.genymobile.scrcpy.Server '4.1' 'scid=0000abcd' 'log_level=info' 'tunnel_forward=true'"
        ));
        assert!(cmd.contains(" 'video_codec_options=a:int=1'\\''b' "), "{cmd}");
        assert!(cmd.contains(" 'send_stream_meta=true' "));
        assert_eq!(p.socket_name(), "scrcpy_0000abcd");
        assert!(cmd.contains(" 'audio=false' ") && !cmd.contains("audio_codec"));
        let cmd = ServerParams { audio: true, ..Default::default() }.command("4.1");
        assert!(cmd.contains(" 'audio=true' ") && cmd.contains(" 'audio_codec=raw' "), "{cmd}");
    }
}
