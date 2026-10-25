//! The automation socket: lets tools on this PC (the `android-controller-mcp`
//! server for Claude, scripts) drive the running app. A Unix socket under
//! `$XDG_RUNTIME_DIR/android-controller/`, readable by this user only.
//!
//! Wire format: one JSON object per line.
//! Request `{"id": 1, "method": "tap", "params": {...}}`, answer
//! `{"id": 1, "result": ...}` or `{"id": 1, "error": "why"}`. Requests on one
//! connection may overlap; answers carry their id.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

/// One request for the device manager.
pub struct ControlRequest {
    pub method: String,
    pub params: Value,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// `$XDG_RUNTIME_DIR/android-controller/control.sock`, else under /tmp
/// (per user).
pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("android-controller").join("control.sock"),
        None => {
            // SAFETY: getuid has no failure mode.
            let uid = unsafe { libc_getuid() };
            PathBuf::from(format!("/tmp/android-controller-{uid}")).join("control.sock")
        }
    }
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// Binds the socket (unless another running app holds it) and serves it
/// until the manager goes away. `clients` counts open connections.
pub async fn serve(path: PathBuf, tx: mpsc::UnboundedSender<ControlRequest>, clients: Arc<AtomicUsize>) {
    let listener = match bind(&path).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("automation socket {}: {e}", path.display());
            return;
        }
    };
    tracing::info!("automation socket at {}", path.display());
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        if tx.is_closed() {
            return;
        }
        let (tx, clients) = (tx.clone(), clients.clone());
        tokio::spawn(async move {
            clients.fetch_add(1, Ordering::Relaxed);
            connection(stream, tx).await;
            clients.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

async fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let dir = path.parent().expect("socket path has a folder");
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "another Android Controller is running"));
        }
        std::fs::remove_file(path)?; // left over from a crash
    }
    let l = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

async fn connection(stream: UnixStream, tx: mpsc::UnboundedSender<ControlRequest>) {
    let (read, mut write) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        while let Some(v) = out_rx.recv().await {
            let mut line = v.to_string();
            line.push('\n');
            if write.write_all(line.as_bytes()).await.is_err() {
                return;
            }
        }
    });
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = out_tx.send(json!({ "id": null, "error": format!("not JSON: {e}") }));
                continue;
            }
        };
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or_default().to_owned();
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
        let (reply, answer) = oneshot::channel();
        if tx.send(ControlRequest { method, params, reply }).is_err() {
            return;
        }
        let out = out_tx.clone();
        tokio::spawn(async move {
            let v = match answer.await {
                Ok(Ok(result)) => json!({ "id": id, "result": result }),
                Ok(Err(error)) => json!({ "id": id, "error": error }),
                Err(_) => json!({ "id": id, "error": "the app closed" }),
            };
            let _ = out.send(v);
        });
    }
    drop(out_tx);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn requests_and_answers_by_id() {
        let dir = std::env::temp_dir().join(format!("ac-control-{}", std::process::id()));
        let path = dir.join("control.sock");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let clients = Arc::new(AtomicUsize::new(0));
        tokio::spawn(serve(path.clone(), tx, clients.clone()));
        tokio::spawn(async move {
            while let Some(r) = rx.recv().await {
                let v = if r.method == "echo" { Ok(r.params) } else { Err(format!("no method {}", r.method)) };
                let _ = r.reply.send(v);
            }
        });
        let mut s = None;
        for _ in 0..50 {
            if let Ok(c) = UnixStream::connect(&path).await {
                s = Some(c);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let s = s.expect("socket up");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "this user only");
        let (r, mut w) = s.into_split();
        w.write_all(b"{\"id\":1,\"method\":\"echo\",\"params\":{\"a\":1}}\n{\"id\":2,\"method\":\"nope\"}\n").await.unwrap();
        let mut lines = BufReader::new(r).lines();
        let mut got = vec![];
        for _ in 0..2 {
            got.push(serde_json::from_str::<Value>(&lines.next_line().await.unwrap().unwrap()).unwrap());
        }
        got.sort_by_key(|v| v["id"].as_u64());
        assert_eq!(got[0], json!({ "id": 1, "result": { "a": 1 } }));
        assert_eq!(got[1], json!({ "id": 2, "error": "no method nope" }));
        assert_eq!(clients.load(Ordering::Relaxed), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
