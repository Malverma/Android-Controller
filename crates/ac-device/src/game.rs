//! The in-game debug add-on (protocol v1, see ACaddon.md): a game built
//! with it listens on the phone's `127.0.0.1:27191` and speaks newline JSON.
//!
//! Game → PC: `{"type": "hello", "protocol": 1, "game", "version", "engine",
//! "package", "commands": [{"name", "args", "help"}]}` on connect, then
//! `{"type": "log", "level", "message", "stack"}` and `{"type": "event",
//! "name", "data"}` whenever they happen, and `{"type": "result", "id",
//! "ok", "result" | "error"}` answers. PC → game: `{"id", "type": "state"}`,
//! `{"id", "type": "command", "name", "args"}`, `{"id", "type": "ping"}`.
//!
//! Over ADB the manager connects to the game itself (`adb` `tcp:` service);
//! without ADB the helper (0.7+) relays it on the link.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use ac_adb::AdbClient;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::api::{CrashKind, CrashView};

pub const PROTOCOL: u64 = 1;
/// Lines kept per phone for `game_logs`.
const KEEP: usize = 2000;
/// Messages and stacks longer than this are cut.
const MAX_TEXT: usize = 8000;
/// The same exception again within this long is not another badge.
const EXCEPTION_REPEAT: Duration = Duration::from_secs(10);
/// How often a phone is checked for a game with the add-on.
pub const RETRY: Duration = Duration::from_secs(3);

/// A request for the game and where its answer goes.
pub struct GameReq {
    pub msg: Value,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// What the connection to a game reports.
#[derive(Debug, Clone, PartialEq)]
pub enum GameEvent {
    /// A `hello`, `log` or `event` line.
    Line(Value),
    /// The game closed the connection (or stopped).
    Gone,
}

/// One log line or event the game sent.
#[derive(Debug, Clone, PartialEq)]
pub struct GameItem {
    pub seq: u64,
    /// The PC's clock, `hh:mm:ss.mmm`.
    pub time: String,
    /// `debug`, `info`, `warn`, `error`, `exception`, or `event`.
    pub level: String,
    pub message: String,
    pub stack: String,
}

/// A phone's game: who it is and what it said.
#[derive(Debug, Default)]
pub struct GameBook {
    pub hello: Option<Value>,
    pub items: VecDeque<GameItem>,
    pub seq: u64,
    last_exception: Option<(String, std::time::Instant)>,
}

pub fn level_rank(level: &str) -> u8 {
    match level {
        "debug" | "verbose" | "trace" => 0,
        "info" | "log" | "event" => 1,
        "warn" | "warning" => 2,
        "error" => 3,
        "exception" | "fatal" | "assert" => 4,
        _ => 1,
    }
}

fn text(v: &Value, k: &str) -> String {
    let s = match v.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    if s.len() <= MAX_TEXT {
        return s;
    }
    let mut at = MAX_TEXT;
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    format!("{}…", &s[..at])
}

impl GameBook {
    /// Takes a line from the game; returns the item it added, if any, and a
    /// crash entry when it is a new exception.
    pub fn take(&mut self, v: &Value, now: chrono::DateTime<chrono::Local>) -> (Option<GameItem>, Option<CrashView>) {
        match v.get("type").and_then(Value::as_str) {
            Some("hello") => {
                self.hello = Some(v.clone());
                (None, None)
            }
            Some(kind @ ("log" | "event")) => {
                self.seq += 1;
                let item = if kind == "event" {
                    let data = v.get("data").filter(|d| !d.is_null()).map(|d| format!(" {d}")).unwrap_or_default();
                    GameItem { seq: self.seq, time: now.format("%H:%M:%S%.3f").to_string(), level: "event".into(), message: format!("{}{data}", text(v, "name")), stack: String::new() }
                } else {
                    let level = v.get("level").and_then(Value::as_str).unwrap_or("info").to_lowercase();
                    GameItem { seq: self.seq, time: now.format("%H:%M:%S%.3f").to_string(), level, message: text(v, "message"), stack: text(v, "stack") }
                };
                self.items.push_back(item.clone());
                if self.items.len() > KEEP {
                    self.items.pop_front();
                }
                let crash = (item.level == "exception").then(|| self.exception(&item, now)).flatten();
                (Some(item), crash)
            }
            _ => (None, None),
        }
    }

    fn exception(&mut self, item: &GameItem, now: chrono::DateTime<chrono::Local>) -> Option<CrashView> {
        let summary = item.message.lines().next().unwrap_or("Exception").trim().to_owned();
        let at = std::time::Instant::now();
        if self.last_exception.as_ref().is_some_and(|(s, t)| *s == summary && at.duration_since(*t) < EXCEPTION_REPEAT) {
            return None;
        }
        self.last_exception = Some((summary.clone(), at));
        let package = self
            .hello
            .as_ref()
            .and_then(|h| h.get("package").or_else(|| h.get("game")))
            .and_then(Value::as_str)
            .unwrap_or("game")
            .to_owned();
        let detail: String = item.stack.lines().take(40).collect::<Vec<_>>().join("\n");
        Some(CrashView { kind: CrashKind::Exception, package, summary, time: now.format("%m-%d %H:%M:%S%.3f").to_string(), detail })
    }

    pub fn gone(&mut self) {
        self.hello = None;
    }

    /// Lines after `since` at `min_level` or above, oldest first, and the
    /// last seq.
    pub fn render(&self, since: u64, min_level: u8) -> (String, u64) {
        let mut out = String::new();
        for i in self.items.iter().filter(|i| i.seq > since && level_rank(&i.level) >= min_level) {
            out.push_str(&line(i));
        }
        (out, self.seq)
    }

    /// `name 1.2 (Unity)`, for the phone list.
    pub fn summary(&self) -> Value {
        match &self.hello {
            Some(h) => json!({ "game": h.get("game"), "version": h.get("version"), "engine": h.get("engine") }),
            None => Value::Null,
        }
    }
}

/// `[12] 14:03:22.120 ERROR message` and the stack, indented.
pub fn line(i: &GameItem) -> String {
    let mut s = format!("[{}] {} {} {}\n", i.seq, i.time, i.level.to_uppercase(), i.message);
    for l in i.stack.lines().filter(|l| !l.trim().is_empty()).take(40) {
        s.push_str("    ");
        s.push_str(l.trim_end());
        s.push('\n');
    }
    s
}

/// The game's answer: `ok` → its `result`, else its `error`.
pub fn result(v: &Value) -> Result<Value, String> {
    if v.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(v.get("error").and_then(Value::as_str).unwrap_or("the game said no").to_owned())
    }
}

/// Keeps connecting to the game on an ADB phone until aborted: reports its
/// lines through `report` (false: stop) and passes requests through.
pub async fn run_adb(adb: AdbClient, serial: String, port: u16, mut reqs: mpsc::UnboundedReceiver<GameReq>, report: impl Fn(GameEvent) -> bool) {
    let mut next_id = 0u64;
    loop {
        if let Ok(stream) = adb.open_service(&serial, &format!("tcp:{port}")).await {
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let mut pending: HashMap<u64, oneshot::Sender<Result<Value, String>>> = HashMap::new();
            let mut greeted = false;
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break };
                        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                        if v.get("type").and_then(Value::as_str) == Some("result") {
                            if let Some(tx) = v.get("id").and_then(Value::as_u64).and_then(|id| pending.remove(&id)) {
                                let _ = tx.send(result(&v));
                            }
                            continue;
                        }
                        greeted |= v.get("type").and_then(Value::as_str) == Some("hello");
                        if !report(GameEvent::Line(v)) {
                            return;
                        }
                    }
                    req = reqs.recv() => {
                        let Some(GameReq { mut msg, reply }) = req else { return };
                        next_id += 1;
                        msg["id"] = json!(next_id);
                        let mut out = msg.to_string();
                        out.push('\n');
                        if write.write_all(out.as_bytes()).await.is_err() {
                            let _ = reply.send(Err("the game closed the connection".into()));
                            break;
                        }
                        pending.insert(next_id, reply);
                    }
                }
            }
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err("the game closed the connection".into()));
            }
            if greeted && !report(GameEvent::Gone) {
                return;
            }
        }
        // No game there: answer waiting requests, then look again.
        let wait = tokio::time::sleep(RETRY);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                _ = &mut wait => break,
                req = reqs.recv() => match req {
                    Some(r) => drop(r.reply.send(Err(no_game(port)))),
                    None => return,
                },
            }
        }
    }
}

pub fn no_game(port: u16) -> String {
    format!(
        "No game with the debug add-on is running on this phone (nothing answers on its port {port}). \
         Start a debug build of the game; see ACaddon.md"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn keeps_logs_and_badges_exceptions_once() {
        let now = chrono::Local.with_ymd_and_hms(2026, 10, 24, 14, 3, 22).unwrap();
        let mut b = GameBook::default();
        let hello = json!({ "type": "hello", "protocol": 1, "game": "Stacker", "version": "1.2", "engine": "Unity", "package": "com.stacker3d.game" });
        assert_eq!(b.take(&hello, now), (None, None));
        assert_eq!(b.summary(), json!({ "game": "Stacker", "version": "1.2", "engine": "Unity" }));
        b.take(&json!({ "type": "log", "level": "info", "message": "level 3 loaded" }), now);
        b.take(&json!({ "type": "event", "name": "coins", "data": { "n": 5 } }), now);
        let ex = json!({ "type": "log", "level": "exception", "message": "NullReferenceException: boom\nmore", "stack": "at Player.Jump()\nat Update()" });
        let (item, crash) = b.take(&ex, now);
        assert_eq!(item.unwrap().seq, 3);
        let crash = crash.expect("a badge");
        assert_eq!((crash.kind, crash.package.as_str(), crash.summary.as_str()), (CrashKind::Exception, "com.stacker3d.game", "NullReferenceException: boom"));
        assert_eq!(crash.detail, "at Player.Jump()\nat Update()");
        assert!(b.take(&ex, now).1.is_none(), "the same one again soon is not another badge");
        let (all, next) = b.render(0, 0);
        assert_eq!(next, 4);
        assert_eq!(
            all.lines().take(3).collect::<Vec<_>>(),
            ["[1] 14:03:22.000 INFO level 3 loaded", "[2] 14:03:22.000 EVENT coins {\"n\":5}", "[3] 14:03:22.000 EXCEPTION NullReferenceException: boom"]
        );
        assert!(all.contains("\n    at Player.Jump()\n"));
        let (errors, _) = b.render(1, level_rank("error"));
        assert_eq!(errors.lines().filter(|l| l.starts_with('[')).count(), 2);
        b.gone();
        assert_eq!(b.summary(), Value::Null);
        assert_eq!(result(&json!({ "ok": true, "result": { "coins": 7 } })), Ok(json!({ "coins": 7 })));
        assert_eq!(result(&json!({ "ok": false, "error": "no level 99" })), Err("no level 99".into()));
    }
}
