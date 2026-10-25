//! Games built with the debug add-on (`crate::game`, ACaddon.md): one
//! connection per ADB phone, the helper's relay for the others; their
//! exceptions become crash badges and their logs go into test runs.

use serde_json::{Value, json};

use super::automation::{Job, Reply, Target, names_of, spawn_fan};
use super::*;
use crate::game::{self, GameBook, GameEvent, GameReq};

/// Waiting for the game's answer, at most.
const GAME_TIMEOUT_MAX: Duration = Duration::from_secs(60);
/// The no-ADB link carries the heartbeat too: shorter there.
const GAME_TIMEOUT_LINK: u64 = 10_000;

/// An ADB phone's game connection: the task, its serial, its request queue.
type GameTask = (JoinHandle<()>, String, mpsc::UnboundedSender<GameReq>);

pub(super) struct Games {
    pub books: Vec<GameBook>,
    pub tasks: Vec<Option<GameTask>>,
}

impl Games {
    pub fn new(slots: usize) -> Self {
        Self { books: (0..slots).map(|_| GameBook::default()).collect(), tasks: (0..slots).map(|_| None).collect() }
    }
}

impl Manager {
    /// One game connection per online ADB phone while automation is on.
    pub(super) fn reconcile_games(&mut self) {
        for i in 0..self.slots.len() {
            let want = self.adb_target(i).filter(|_| self.cfg.automation.enabled);
            let keep = matches!((&self.games.tasks[i], &want), (Some((t, s, _)), Some((serial, ..))) if !t.is_finished() && s == serial);
            if keep {
                continue;
            }
            if let Some((t, ..)) = self.games.tasks[i].take() {
                t.abort();
                self.games.books[i].gone();
            }
            if self.direct[i].is_none() && want.is_none() {
                self.games.books[i].gone();
            }
            let Some((serial, ..)) = want else { continue };
            let (tx, rx) = mpsc::unbounded_channel();
            let (adb, msgs, slot, port) = (self.adb.clone(), self.tx.clone(), i as SlotId + 1, self.cfg.automation.game_port);
            let s = serial.clone();
            let task = tokio::spawn(game::run_adb(adb, serial.clone(), port, rx, move |ev| msgs.send(Msg::Game { slot, serial: s.clone(), ev }).is_ok()));
            self.games.tasks[i] = Some((task, serial, tx));
        }
    }

    /// A line from (or the end of) the game on an ADB phone.
    pub(super) fn on_game(&mut self, slot: SlotId, serial: &str, ev: GameEvent) {
        let Some(i) = self.idx(slot) else { return };
        if !self.games.tasks[i].as_ref().is_some_and(|(_, s, _)| s == serial) {
            return; // an old connection
        }
        self.game_event(i, ev);
    }

    /// The helper's relay (no ADB): `{"hello", "news": [...], "gone"}`.
    pub(super) fn on_game_news(&mut self, slot: SlotId, conn: u64, news: Value) {
        let Some(i) = self.idx(slot) else { return };
        if !self.direct[i].as_ref().is_some_and(|l| l.conn == conn) {
            return;
        }
        if news.get("gone").and_then(Value::as_bool).unwrap_or(false) {
            self.game_event(i, GameEvent::Gone);
        }
        if let Some(h) = news.get("hello").filter(|h| h.is_object()) {
            self.game_event(i, GameEvent::Line(h.clone()));
        }
        for item in news.get("news").and_then(Value::as_array).into_iter().flatten() {
            self.game_event(i, GameEvent::Line(item.clone()));
        }
    }

    fn game_event(&mut self, i: usize, ev: GameEvent) {
        let slot = i as SlotId + 1;
        let v = match ev {
            GameEvent::Gone => {
                if let Some(h) = self.games.books[i].hello.take() {
                    tracing::info!(slot, "game add-on gone: {}", h.get("game").and_then(serde_json::Value::as_str).unwrap_or("game"));
                }
                return;
            }
            GameEvent::Line(v) => v,
        };
        if v.get("type").and_then(Value::as_str) == Some("hello") {
            let name = v.get("game").and_then(Value::as_str).unwrap_or("a game");
            tracing::info!(slot, "game add-on connected: {name} {}", v.get("version").and_then(serde_json::Value::as_str).unwrap_or_default());
            if v.get("protocol").and_then(Value::as_u64).is_some_and(|p| p > game::PROTOCOL) {
                tracing::warn!(slot, "{name} speaks add-on protocol {}; this app knows {}", v["protocol"], game::PROTOCOL);
            }
        }
        let (item, crash) = self.games.books[i].take(&v, chrono::Local::now());
        if let Some(item) = item
            && let Some(p) = self.run.as_ref().and_then(|r| r.phones[i].as_ref())
        {
            append(&p.dir.join("game.txt"), &game::line(&item));
        }
        if let Some(crash) = crash {
            self.on_log(LogEvent::Crash(slot, crash));
        }
    }

    pub(super) fn game_summary(&self, i: usize) -> Value {
        self.games.books[i].summary()
    }

    pub(super) fn game_control(&mut self, method: &str, p: &Value, ts: Vec<Target>, reply: Reply) -> Result<(), (Reply, String)> {
        let port = self.cfg.automation.game_port;
        let names = names_of(&ts);
        let jobs: Vec<Job> = match method {
            "game_info" => ts
                .iter()
                .map(|t| {
                    let hello = self.idx(t.slot).and_then(|i| self.games.books[i].hello.clone());
                    Box::pin(async move { hello.ok_or_else(|| game::no_game(port)) }) as Job
                })
                .collect(),
            "game_logs" => {
                let since = p.get("since").and_then(Value::as_u64).unwrap_or(0);
                let level = game::level_rank(p.get("level").and_then(Value::as_str).unwrap_or("debug"));
                ts.iter()
                    .map(|t| {
                        let out = self.idx(t.slot).map(|i| {
                            let b = &self.games.books[i];
                            let (text, next) = b.render(since, level);
                            json!({ "text": text, "next": next, "connected": b.hello.is_some() })
                        });
                        Box::pin(async move { out.ok_or_else(|| "gone".to_owned()) }) as Job
                    })
                    .collect()
            }
            "game_state" | "game_command" => {
                let msg = if method == "game_state" {
                    json!({ "type": "state" })
                } else {
                    let Some(name) = p.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                        return Err((reply, "game_command needs name (game_info lists the game's commands)".into()));
                    };
                    json!({ "type": "command", "name": name, "args": p.get("args").cloned().filter(Value::is_object).unwrap_or_else(|| json!({})) })
                };
                let ms = p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(10_000);
                ts.iter().map(|t| self.game_request(t, msg.clone(), ms)).collect()
            }
            _ => return Err((reply, format!("unknown method {method}"))),
        };
        spawn_fan(names, jobs, reply);
        Ok(())
    }

    /// Sends `msg` to the game on `t` and waits for its answer.
    fn game_request(&self, t: &Target, msg: Value, ms: u64) -> Job {
        let port = self.cfg.automation.game_port;
        let limit = Duration::from_millis(ms).min(GAME_TIMEOUT_MAX);
        let i = self.idx(t.slot);
        if let Some((_, _, queue)) = i.and_then(|i| self.games.tasks[i].as_ref()) {
            let (tx, rx) = oneshot::channel();
            let sent = queue.send(GameReq { msg, reply: tx }).is_ok();
            return Box::pin(async move {
                if !sent {
                    return Err(game::no_game(port));
                }
                match tokio::time::timeout(limit, rx).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(_)) => Err("the game connection closed".into()),
                    Err(_) => Err(format!("the game did not answer within {} ms", limit.as_millis())),
                }
            });
        }
        let link = t.link.clone();
        Box::pin(async move {
            let Some((cmds, version)) = link else { return Err(game::no_game(port)) };
            if version < ac_direct::HELPER_V7 {
                return Err("Update Wireless Debugging Keeper on this phone to 0.7 to reach games without ADB".into());
            }
            let (tx, rx) = oneshot::channel();
            let req = json!({ "op": "game", "msg": msg, "timeout_ms": ms.min(GAME_TIMEOUT_LINK) });
            cmds.send(LinkCmd::Dev { req, reply: tx }).map_err(|_| "the phone disconnected".to_owned())?;
            let v = rx.await.map_err(|_| "the phone disconnected".to_owned())??;
            if let Some(e) = v.get("error").and_then(Value::as_str) {
                return Err(e.to_owned());
            }
            game::result(v.get("reply").unwrap_or(&Value::Null))
        })
    }
}
