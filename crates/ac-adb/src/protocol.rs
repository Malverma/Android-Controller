//! Smart-socket framing shared by every ADB server request.
//!
//! A request is `<4 lowercase hex digits length><payload>`. The server answers
//! with `OKAY`, or `FAIL<4 hex length><message>`. Many host services follow
//! `OKAY` with a hex-length-prefixed string.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::AdbError;

/// Largest payload the ADB server accepts in a single request.
pub const MAX_PAYLOAD: usize = 0xFFFF;

pub fn encode_request(payload: &str) -> Result<Vec<u8>, AdbError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(AdbError::Protocol(format!(
            "request too long ({} bytes)",
            payload.len()
        )));
    }
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(format!("{:04x}", payload.len()).as_bytes());
    buf.extend_from_slice(payload.as_bytes());
    Ok(buf)
}

pub async fn send_request<S: AsyncWrite + Unpin>(s: &mut S, payload: &str) -> Result<(), AdbError> {
    let buf = encode_request(payload)?;
    s.write_all(&buf).await?;
    s.flush().await?;
    Ok(())
}

/// Reads `OKAY` or `FAIL<msg>`.
pub async fn read_status<S: AsyncRead + Unpin>(s: &mut S) -> Result<(), AdbError> {
    let mut status = [0u8; 4];
    s.read_exact(&mut status).await?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => Err(AdbError::Fail(read_hex_string(s).await?)),
        other => Err(AdbError::Protocol(format!(
            "unexpected status {:?}",
            String::from_utf8_lossy(other)
        ))),
    }
}

pub fn parse_hex_len(digits: &[u8; 4]) -> Result<usize, AdbError> {
    let text = std::str::from_utf8(digits)
        .map_err(|_| AdbError::Protocol("non-ASCII length prefix".into()))?;
    usize::from_str_radix(text, 16)
        .map_err(|_| AdbError::Protocol(format!("invalid length prefix {text:?}")))
}

/// Reads a hex-length-prefixed string. Some host services (e.g. `host:pair`)
/// answer `OKAY` and then report failure with a nested `FAIL<len><msg>`, so a
/// `FAIL` here is surfaced as `AdbError::Fail`.
pub async fn read_hex_string<S: AsyncRead + Unpin>(s: &mut S) -> Result<String, AdbError> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await?;
    if &len == b"FAIL" {
        s.read_exact(&mut len).await?;
        let mut msg = vec![0u8; parse_hex_len(&len)?];
        s.read_exact(&mut msg).await?;
        return Err(AdbError::Fail(String::from_utf8_lossy(&msg).into_owned()));
    }
    let len = parse_hex_len(&len)?;
    let mut buf = vec![0u8; len];
    s.read_exact(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

pub async fn read_to_end_string<S: AsyncRead + Unpin>(s: &mut S) -> Result<String, AdbError> {
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_length_as_lowercase_hex() {
        assert_eq!(encode_request("host:version").unwrap(), b"000chost:version");
        let long = "x".repeat(0xab);
        assert_eq!(&encode_request(&long).unwrap()[..4], b"00ab");
    }

    #[test]
    fn rejects_oversize_requests() {
        assert!(encode_request(&"x".repeat(MAX_PAYLOAD + 1)).is_err());
    }

    #[tokio::test]
    async fn parses_okay_and_fail() {
        let mut ok: &[u8] = b"OKAY";
        read_status(&mut ok).await.unwrap();

        let mut fail: &[u8] = b"FAIL0010device not found";
        match read_status(&mut fail).await {
            Err(AdbError::Fail(msg)) => assert_eq!(msg, "device not found"),
            other => panic!("expected Fail, got {other:?}"),
        }

        let mut junk: &[u8] = b"WHAT";
        assert!(matches!(read_status(&mut junk).await, Err(AdbError::Protocol(_))));
    }

    #[tokio::test]
    async fn reads_hex_string() {
        let mut s: &[u8] = b"0005hello";
        assert_eq!(read_hex_string(&mut s).await.unwrap(), "hello");
        let mut bad: &[u8] = b"zzzzhello";
        assert!(read_hex_string(&mut bad).await.is_err());
    }

    #[tokio::test]
    async fn nested_fail_after_okay() {
        // Captured from android-tools 37 for a failed `host:pair`.
        let mut s: &[u8] = b"OKAYFAIL0027Failed: Unable to start pairing client.";
        read_status(&mut s).await.unwrap();
        match read_hex_string(&mut s).await {
            Err(AdbError::Fail(msg)) => assert_eq!(msg, "Failed: Unable to start pairing client."),
            other => panic!("expected Fail, got {other:?}"),
        }
    }
}
