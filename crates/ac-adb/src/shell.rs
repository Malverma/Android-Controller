//! Shell protocol v2 packet codec and remote-shell quoting.
//!
//! A v2 packet is `u8 id, u32 little-endian length, data[length]`.

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::AdbError;

pub const ID_STDIN: u8 = 0;
pub const ID_STDOUT: u8 = 1;
pub const ID_STDERR: u8 = 2;
pub const ID_EXIT: u8 = 3;
pub const ID_CLOSE_STDIN: u8 = 4;
pub const ID_WINDOW_SIZE_CHANGE: u8 = 5;

/// Refuse absurd packets from a confused peer.
const MAX_PACKET: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the device lacks `shell_v2` and no exit status is available.
    pub exit_code: Option<u8>,
}

impl ShellOutput {
    pub fn success(&self) -> bool {
        self.exit_code.is_none_or(|c| c == 0)
    }
}

pub fn encode_packet(id: u8, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5 + data.len());
    buf.push(id);
    buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
    buf.extend_from_slice(data);
    buf
}

/// Reads one packet; `Ok(None)` on clean EOF at a packet boundary.
pub async fn read_packet<S: AsyncRead + Unpin>(s: &mut S) -> Result<Option<(u8, Vec<u8>)>, AdbError> {
    let mut header = [0u8; 5];
    match s.read_exact(&mut header[..1]).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    s.read_exact(&mut header[1..]).await?;
    let len = u32::from_le_bytes(header[1..5].try_into().unwrap());
    if len > MAX_PACKET {
        return Err(AdbError::Protocol(format!("shell v2 packet too large ({len})")));
    }
    let mut data = vec![0u8; len as usize];
    s.read_exact(&mut data).await?;
    Ok(Some((header[0], data)))
}

/// Collects stdout/stderr until the exit packet (or EOF).
pub async fn collect_v2<S: AsyncRead + Unpin>(s: &mut S) -> Result<ShellOutput, AdbError> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    while let Some((id, data)) = read_packet(s).await? {
        match id {
            ID_STDOUT => stdout.extend_from_slice(&data),
            ID_STDERR => stderr.extend_from_slice(&data),
            ID_EXIT => {
                exit_code = Some(data.first().copied().unwrap_or(0));
                break;
            }
            _ => {}
        }
    }
    Ok(ShellOutput {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        exit_code,
    })
}

/// Quotes one argument for the device's `/system/bin/sh`.
///
/// Wraps in single quotes and rewrites embedded `'` as `'\''`. NUL bytes
/// cannot be represented in a shell argument and are rejected.
pub fn quote(arg: &str) -> Result<String, AdbError> {
    if arg.contains('\0') {
        return Err(AdbError::Protocol("NUL byte in shell argument".into()));
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_arguments() {
        assert_eq!(quote("plain").unwrap(), "'plain'");
        assert_eq!(quote("a b").unwrap(), "'a b'");
        assert_eq!(quote("it's").unwrap(), r"'it'\''s'");
        assert_eq!(quote("$(rm -rf /)").unwrap(), "'$(rm -rf /)'");
        assert_eq!(quote("").unwrap(), "''");
        assert!(quote("a\0b").is_err());
    }

    #[tokio::test]
    async fn collects_v2_output_and_exit_code() {
        let mut bytes = Vec::new();
        bytes.extend(encode_packet(ID_STDOUT, b"hello "));
        bytes.extend(encode_packet(ID_STDERR, b"warn"));
        bytes.extend(encode_packet(ID_STDOUT, b"world"));
        bytes.extend(encode_packet(ID_EXIT, &[3]));
        let out = collect_v2(&mut bytes.as_slice()).await.unwrap();
        assert_eq!(out.stdout, "hello world");
        assert_eq!(out.stderr, "warn");
        assert_eq!(out.exit_code, Some(3));
        assert!(!out.success());
    }

    #[tokio::test]
    async fn eof_without_exit_packet_is_not_an_error() {
        let bytes = encode_packet(ID_STDOUT, b"partial");
        let out = collect_v2(&mut bytes.as_slice()).await.unwrap();
        assert_eq!(out.stdout, "partial");
        assert_eq!(out.exit_code, None);
    }

    #[tokio::test]
    async fn rejects_oversize_packet() {
        let mut bytes = vec![ID_STDOUT];
        bytes.extend_from_slice(&(MAX_PACKET + 1).to_le_bytes());
        assert!(collect_v2(&mut bytes.as_slice()).await.is_err());
    }
}
