//! The automation API behind the control socket (`crate::control`): what
//! each method does. Claude (through `android-controller-mcp`) and scripts
//! drive the phones with these.
//!
//! Every method takes `params.phone`: a slot number, a phone's name (or part
//! of it), or `"all"` where several phones make sense. Without it the only
//! connected phone is meant. Coordinates are the phone's screen pixels.
//! With several phones the answer is a list of `{phone, slot, result|error}`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::*;
use crate::control::ControlRequest;
use crate::uitree::{self, Node, Query};

/// One phone's share of a request, run off the manager's task.
pub(super) type Job = Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;
pub(super) type Reply = oneshot::Sender<Result<Value, String>>;

/// The helper's automation socket for ADB phones (`InputService`).
const DEV_SOCKET: &str = "localabstract:android-controller-dev";
/// Output longer than this is cut (keeping the end) so answers stay readable.
const MAX_OUTPUT: usize = 30_000;
/// Where `uiautomator dump` writes the screen (ADB phones without the helper).
const UIAUTOMATOR_XML: &str = "/data/local/tmp/android-controller-ui.xml";
/// Destructive actions wait this long for the user's answer.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);
/// `perf` keeps sampling this long after it was last asked.
const CONTROL_PERF_FOR: Duration = Duration::from_secs(120);
/// Crashes kept for `crashes`.
const CRASH_LOG: usize = 200;

/// What a job needs to reach one phone.
#[derive(Clone)]
pub(super) struct Target {
    pub slot: SlotId,
    pub name: String,
    /// `(serial, shell_v2, sdk)` over ADB.
    pub adb: Option<(String, bool, u32)>,
    /// The helper link (no ADB) and the helper's version.
    pub link: Option<(mpsc::UnboundedSender<LinkCmd>, u32)>,
    pub io: Arc<SlotIo>,
}

/// Shared by jobs.
#[derive(Clone)]
pub(super) struct Ctx {
    pub adb: AdbClient,
    /// ADB serials whose helper touch control was switched on already.
    pub enabled: Arc<Mutex<HashSet<String>>>,
}

/// Automation state kept by the manager.
#[derive(Default)]
pub(super) struct Automation {
    pub clients: Arc<std::sync::atomic::AtomicUsize>,
    pub last_clients: usize,
    pub next_id: u64,
    pub confirms: std::collections::HashMap<u64, oneshot::Sender<bool>>,
    pub frames: std::collections::HashMap<u64, oneshot::Sender<Result<(), String>>>,
    pub crashes: std::collections::VecDeque<(u64, SlotId, String, CrashView)>,
    pub crash_seq: u64,
    pub last_perf: Vec<Option<(Instant, PerfSample)>>,
    pub perf_until: Option<Instant>,
    /// Per slot: the screen size the helper reported (no ADB).
    pub screen: Vec<Option<(u32, u32)>>,
    pub enabled: Arc<Mutex<HashSet<String>>>,
}

impl Automation {
    pub fn new(slots: usize) -> Self {
        Self { last_perf: vec![None; slots], screen: vec![None; slots], ..Self::default() }
    }
}

/// Why the helper could not answer: not there (try another way) or it said no.
enum DevErr {
    Unavailable(String),
    Failed(String),
}

impl From<DevErr> for String {
    fn from(e: DevErr) -> String {
        match e {
            DevErr::Unavailable(s) | DevErr::Failed(s) => s,
        }
    }
}

impl Manager {
    pub(super) fn on_control(&mut self, req: ControlRequest) {
        self.check_clients();
        let ControlRequest { method, params, reply } = req;
        tracing::debug!(%method, "automation request");
        match self.control(&method, &params, reply) {
            Ok(()) => {}
            Err((reply, e)) => {
                let _ = reply.send(Err(e));
            }
        }
    }

    pub(super) fn check_clients(&mut self) {
        let n = self.auto.clients.load(std::sync::atomic::Ordering::Relaxed);
        if n != self.auto.last_clients {
            self.auto.last_clients = n;
            self.send(UiEvent::Automation { clients: n });
        }
    }

    fn ctx(&self) -> Ctx {
        Ctx { adb: self.adb.clone(), enabled: self.auto.enabled.clone() }
    }

    fn next_id(&mut self) -> u64 {
        self.auto.next_id += 1;
        self.auto.next_id
    }

    /// Answers now, or hands `reply` to a task that will.
    fn control(&mut self, method: &str, p: &Value, reply: Reply) -> Result<(), (Reply, String)> {
        macro_rules! bail {
            ($e:expr) => {
                return Err((reply, $e))
            };
        }
        macro_rules! targets {
            ($many:expr) => {
                match self.targets(p, $many) {
                    Ok(t) => t,
                    Err(e) => bail!(e),
                }
            };
        }
        let ctx = self.ctx();
        match method {
            "phones" => {
                let _ = reply.send(Ok(self.phones()));
            }
            "screenshot" => {
                let ts = targets!(true);
                let names = names_of(&ts);
                let save = p.get("save").and_then(Value::as_bool).unwrap_or(false);
                let jobs = ts.into_iter().map(|t| self.screenshot_job(t, save)).collect();
                spawn_fan(names, jobs, reply);
            }
            "tap" => {
                let ts = targets!(true);
                let names = names_of(&ts);
                let query = Query::from_params(p);
                let xy = (num(p, "x"), num(p, "y"));
                let hold = if p.get("long").and_then(Value::as_bool).unwrap_or(false) { 800 } else { 0 };
                let hold = p.get("duration_ms").and_then(Value::as_u64).map_or(hold, |d| d.min(10_000) as u32);
                if query.is_none() && (xy.0.is_none() || xy.1.is_none()) {
                    bail!("say where: x and y (phone pixels), or text / id / desc of the element".into());
                }
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let (ctx, q) = (ctx.clone(), query.clone());
                        Box::pin(async move {
                            let (x, y, element) = match q {
                                Some(q) => {
                                    let (nodes, _, _) = tree(&ctx, &t).await?;
                                    let hit = pick(&nodes, &q)?;
                                    let (x, y) = hit.center();
                                    (x, y, Some(hit.line()))
                                }
                                None => (xy.0.unwrap_or(0.0) as i32, xy.1.unwrap_or(0.0) as i32, None),
                            };
                            tap_at(&ctx, &t, x, y, hold).await?;
                            Ok(json!({ "x": x, "y": y, "element": element }))
                        }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            "swipe" => {
                let ts = targets!(true);
                let names = names_of(&ts);
                let (Some(x1), Some(y1), Some(x2), Some(y2)) = (num(p, "x1"), num(p, "y1"), num(p, "x2"), num(p, "y2")) else {
                    bail!("swipe needs x1, y1, x2, y2 (phone pixels)".into());
                };
                let ms = p.get("duration_ms").and_then(Value::as_u64).unwrap_or(300).clamp(10, 10_000) as u32;
                let pts = [x1 as i32, y1 as i32, x2 as i32, y2 as i32];
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let ctx = ctx.clone();
                        Box::pin(async move {
                            swipe(&ctx, &t, pts, ms).await?;
                            Ok(json!("swiped"))
                        }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            "type" => {
                let ts = targets!(true);
                let names = names_of(&ts);
                let Some(text) = p.get("text").and_then(Value::as_str).map(str::to_owned) else { bail!("type needs text".into()) };
                let enter = p.get("enter").and_then(Value::as_bool).unwrap_or(false);
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let (ctx, text) = (ctx.clone(), text.clone());
                        Box::pin(async move {
                            type_text(&ctx, &t, &text).await?;
                            if enter {
                                key(&ctx, &t, 66).await?;
                            }
                            Ok(json!("typed"))
                        }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            "key" => {
                let ts = targets!(true);
                let names = names_of(&ts);
                let Some(code) = p.get("key").and_then(keycode) else {
                    bail!("key: back, home, enter, recents, power, delete, tab, escape, up/down/left/right, volume_up/volume_down, or an Android key code".into());
                };
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let ctx = ctx.clone();
                        Box::pin(async move {
                            key(&ctx, &t, code).await?;
                            Ok(json!("pressed"))
                        }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            "ui_tree" | "find" => {
                let ts = targets!(false);
                let names = names_of(&ts);
                let all = p.get("all").and_then(Value::as_bool).unwrap_or(false);
                let query = Query::from_params(p);
                if method == "find" && query.is_none() {
                    bail!("find needs text, id or desc".into());
                }
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let (ctx, q) = (ctx.clone(), query.clone());
                        Box::pin(async move {
                            let (nodes, source, pkg) = tree(&ctx, &t).await?;
                            match q {
                                Some(q) => {
                                    let hits: Vec<String> = uitree::find(&nodes, &q).iter().map(|n| n.line()).collect();
                                    Ok(json!({ "source": source, "package": pkg, "matches": hits }))
                                }
                                None => Ok(json!({ "source": source, "package": pkg, "text": uitree::render(&nodes, all) })),
                            }
                        }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            "wait_for" => {
                let ts = targets!(false);
                let names = names_of(&ts);
                let Some(q) = Query::from_params(p) else { bail!("wait_for needs text, id or desc".into()) };
                let gone = p.get("gone").and_then(Value::as_bool).unwrap_or(false);
                let limit = Duration::from_millis(p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(10_000).clamp(100, 120_000));
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let (ctx, q) = (ctx.clone(), q.clone());
                        Box::pin(async move { wait_for(&ctx, &t, &q, gone, limit).await }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            "events" | "auto_allow" => {
                let ts = targets!(true);
                let names = names_of(&ts);
                let req = if method == "events" {
                    json!({ "op": "events", "since": p.get("since").and_then(Value::as_u64).unwrap_or(0) })
                } else {
                    json!({ "op": "auto_allow", "on": p.get("on").and_then(Value::as_bool).unwrap_or(true) })
                };
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let (ctx, req) = (ctx.clone(), req.clone());
                        Box::pin(async move { Ok(dev(&ctx, &t, req).await?) }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            "install" => {
                let ts = targets!(true);
                let Some(path) = p.get("path").and_then(Value::as_str).map(PathBuf::from) else { bail!("install needs path (an .apk on this PC)".into()) };
                if !path.is_file() {
                    bail!(format!("{} is not a file", path.display()));
                }
                if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("apk")) {
                    bail!("install takes an .apk file".into());
                }
                let (mut jobs, mut names) = (Vec::new(), Vec::new());
                for t in ts {
                    let Some(i) = self.idx(t.slot) else { continue };
                    let Some(id) = self.slots[i].device_id().map(str::to_owned) else { continue };
                    names.push((t.slot, t.name.clone()));
                    let mut results = self.transfers.install_results();
                    self.transfers.submit(t.slot, id, t.name.clone(), vec![path.clone()], true);
                    jobs.push(Box::pin(async move {
                        let wait = async {
                            loop {
                                match results.recv().await {
                                    Ok((slot, ok, message)) if slot == t.slot => return if ok { Ok(json!(message)) } else { Err(message) },
                                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                    Err(_) => return Err("the app closed".to_owned()),
                                }
                            }
                        };
                        tokio::time::timeout(Duration::from_secs(600), wait).await.map_err(|_| "the install took over 10 minutes".to_owned())?
                    }) as Job);
                }
                spawn_fan(names, jobs, reply);
            }
            "app" => {
                let ts = targets!(true);
                let names = names_of(&ts);
                let Some(pkg) = p.get("package").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned) else {
                    bail!("app needs package".into())
                };
                let action = p.get("action").and_then(Value::as_str).unwrap_or_default();
                let actions = match action {
                    "launch" | "start" => vec![AppAction::Launch(pkg.clone())],
                    "stop" => vec![AppAction::Stop(pkg.clone())],
                    "restart" => vec![AppAction::Stop(pkg.clone()), AppAction::Launch(pkg.clone())],
                    "clear_data" | "clear" => vec![AppAction::ClearData(pkg.clone())],
                    "uninstall" => vec![AppAction::Uninstall(pkg.clone())],
                    _ => bail!("action: launch, stop, restart, clear_data or uninstall".into()),
                };
                let destructive = actions.iter().any(|a| matches!(a, AppAction::ClearData(_) | AppAction::Uninstall(_)));
                let jobs: Vec<Job> = ts
                    .into_iter()
                    .map(|t| {
                        let (ctx, actions) = (ctx.clone(), actions.clone());
                        Box::pin(async move {
                            let mut said = Vec::new();
                            for a in actions {
                                said.push(app(&ctx, &t, a).await?);
                            }
                            Ok(json!(said.join("; ")))
                        }) as Job
                    })
                    .collect();
                let confirm = (destructive && self.cfg.automation.confirm_destructive).then(|| {
                    let what = if action.starts_with("clear") { "Clear the data of" } else { "Uninstall" };
                    self.confirm(format!("{what} {pkg}?"), format!("An automation client (e.g. Claude) asks to do this on {}.", names.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")))
                });
                tokio::spawn(async move {
                    if let Some(c) = confirm
                        && !c.await
                    {
                        let _ = reply.send(Err("The user said no (or did not answer) on the PC".into()));
                        return;
                    }
                    let _ = reply.send(fan(names, jobs).await);
                });
            }
            "packages" => {
                let ts = targets!(false);
                let names = names_of(&ts);
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let ctx = ctx.clone();
                        Box::pin(async move { Ok(json!(packages(&ctx, &t).await?)) }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            "crashes" => {
                let since = p.get("since").and_then(Value::as_u64).unwrap_or(0);
                let list: Vec<Value> = self
                    .auto
                    .crashes
                    .iter()
                    .filter(|(seq, ..)| *seq > since)
                    .map(|(seq, slot, phone, c)| {
                        json!({
                            "seq": seq, "slot": slot, "phone": phone, "kind": c.kind.label(), "package": c.package,
                            "summary": c.summary, "time": c.time, "detail": c.detail,
                        })
                    })
                    .collect();
                let watching = self.cfg.testing.watch_crashes;
                let _ = reply.send(Ok(json!({ "crashes": list, "next": self.auto.crash_seq, "watching": watching })));
            }
            "perf" => {
                let ts = targets!(true);
                self.auto.perf_until = Some(Instant::now() + CONTROL_PERF_FOR);
                let out: Vec<Value> = ts
                    .iter()
                    .map(|t| {
                        let s = self.idx(t.slot).and_then(|i| self.auto.last_perf[i].as_ref());
                        match s {
                            Some((at, s)) => json!({
                                "phone": t.name, "age_s": at.elapsed().as_secs(), "fps": round(s.fps), "cpu_pct": round(s.cpu_pct),
                                "mem_used_mb": s.mem_used_mb, "mem_total_mb": s.mem_total_mb, "app": s.app, "app_mem_mb": s.app_mem_mb,
                                "cpu_temp_c": round(s.cpu_temp_c), "battery_temp_c": round(s.battery_temp_c),
                            }),
                            None => json!({ "phone": t.name, "pending": "sampling started: ask again in a few seconds" }),
                        }
                    })
                    .collect();
                let _ = reply.send(Ok(if out.len() == 1 { out.into_iter().next().unwrap_or_default() } else { json!(out) }));
            }
            "logcat" => {
                let ts = targets!(false);
                let names = names_of(&ts);
                let lines = p.get("lines").and_then(Value::as_u64).unwrap_or(200).clamp(1, 5000);
                let pkg = p.get("package").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
                let grep = p.get("grep").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_lowercase);
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let (ctx, pkg, grep) = (ctx.clone(), pkg.clone(), grep.clone());
                        Box::pin(async move { logcat(&ctx, &t, lines, pkg.as_deref(), grep.as_deref()).await }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            "shell" => {
                let ts = targets!(true);
                let names = names_of(&ts);
                let Some(cmd) = p.get("command").and_then(Value::as_str).map(str::to_owned) else { bail!("shell needs command".into()) };
                let limit = Duration::from_millis(p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(30_000).clamp(100, 300_000));
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let (ctx, cmd) = (ctx.clone(), cmd.clone());
                        Box::pin(async move {
                            let (serial, v2, _) = t.adb.clone().ok_or_else(|| no_adb(&t, "shell commands"))?;
                            let out = tokio::time::timeout(limit, ctx.adb.shell(&serial, &cmd, v2))
                                .await
                                .map_err(|_| format!("timed out after {} ms", limit.as_millis()))?
                                .map_err(|e| e.to_string())?;
                            Ok(json!({ "stdout": tail(&out.stdout), "stderr": tail(&out.stderr), "exit": out.exit_code }))
                        }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            "run_start" => {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("Claude").to_owned();
                self.start_run(&name);
                self.reconcile_testing();
                let r = self.run_view();
                if !r.active {
                    bail!("could not start the test run (see the app)".into());
                }
                let _ = reply.send(Ok(json!({ "dir": r.dir, "name": r.name })));
            }
            "run_stop" => {
                let r = self.run_view();
                if !r.active {
                    bail!("no test run is running".into());
                }
                self.stop_run();
                let _ = reply.send(Ok(json!({ "dir": r.dir, "crashes": r.crashes, "summary": r.dir.join("summary.txt") })));
            }
            "record" => {
                let ts = targets!(true);
                let on = p.get("on").and_then(Value::as_bool).unwrap_or(true);
                let slots: Vec<SlotId> = ts.iter().map(|t| t.slot).collect();
                self.record(slots, on);
                let state: Vec<Value> = ts.iter().map(|t| json!({ "phone": t.name, "recording": t.io.mailbox.is_recording() })).collect();
                let _ = reply.send(Ok(json!(state)));
            }
            "settings" => {
                let ts = targets!(true);
                let names = names_of(&ts);
                let anim = p.get("animations").and_then(Value::as_bool);
                let dark = p.get("dark").and_then(Value::as_bool);
                let font = p.get("font_scale").and_then(Value::as_f64);
                if anim.is_none() && dark.is_none() && font.is_none() {
                    bail!("settings: animations (true/false), dark (true/false), font_scale (e.g. 1.0, 1.3)".into());
                }
                let jobs = ts
                    .into_iter()
                    .map(|t| {
                        let ctx = ctx.clone();
                        Box::pin(async move { settings(&ctx, &t, anim, dark, font).await }) as Job
                    })
                    .collect();
                spawn_fan(names, jobs, reply);
            }
            m if m.starts_with("game_") => {
                let ts = targets!(m != "game_command");
                return self.game_control(m, p, ts, reply);
            }
            _ => bail!(format!("unknown method {method}")),
        }
        Ok(())
    }

    /// The phones a request is for.
    fn targets(&self, p: &Value, many: bool) -> Result<Vec<Target>, String> {
        let online: Vec<usize> = (0..self.slots.len()).filter(|&i| matches!(self.slots[i].phase, Phase::Online { .. })).collect();
        let names = || online.iter().map(|&i| format!("{} ({})", self.title(i), i + 1)).collect::<Vec<_>>().join(", ");
        let picked: Vec<usize> = match p.get("phone") {
            None | Some(Value::Null) => match online.len() {
                0 => return Err("No phone is connected to Android Controller".into()),
                1 => online.clone(),
                _ => return Err(format!("Several phones are connected; say which (phone: name, slot number or \"all\"): {}", names())),
            },
            Some(Value::Number(n)) => {
                let i = n.as_u64().unwrap_or(0) as usize;
                if !online.contains(&i.wrapping_sub(1)) {
                    return Err(format!("No connected phone in slot {i}. Connected: {}", names()));
                }
                vec![i - 1]
            }
            Some(Value::String(s)) if s.eq_ignore_ascii_case("all") => {
                if !many {
                    return Err("This works on one phone at a time: name one".into());
                }
                if online.is_empty() {
                    return Err("No phone is connected to Android Controller".into());
                }
                online.clone()
            }
            Some(Value::String(s)) => {
                if let Ok(n) = s.trim().parse::<usize>() {
                    return self.targets(&json!({ "phone": n }), many);
                }
                let want = s.trim().to_lowercase();
                let exact: Vec<usize> = online.iter().copied().filter(|&i| self.title(i).to_lowercase() == want).collect();
                let hits = if exact.is_empty() {
                    online
                        .iter()
                        .copied()
                        .filter(|&i| {
                            let s = &self.slots[i];
                            let model = s.device.as_ref().map(|d| d.model.to_lowercase()).unwrap_or_default();
                            self.title(i).to_lowercase().contains(&want) || model.contains(&want) || s.device_id() == Some(want.as_str())
                        })
                        .collect()
                } else {
                    exact
                };
                match hits.len() {
                    0 => return Err(format!("No connected phone called {s:?}. Connected: {}", names())),
                    1 => hits,
                    _ if many => hits,
                    _ => return Err(format!("{s:?} matches several phones: be more exact. Connected: {}", names())),
                }
            }
            Some(_) => return Err("phone: a name, slot number or \"all\"".into()),
        };
        Ok(picked.into_iter().map(|i| self.target(i)).collect())
    }

    fn target(&self, i: usize) -> Target {
        Target {
            slot: i as SlotId + 1,
            name: self.title(i),
            adb: self.adb_target(i),
            link: self.direct[i].as_ref().map(|l| (l.cmds.clone(), l.hello.version)),
            io: self.io[i].clone(),
        }
    }

    fn phones(&self) -> Value {
        let list: Vec<Value> = (0..self.slots.len())
            .filter_map(|i| {
                let v = self.views[i].as_ref()?;
                if v.phase == PhaseView::Empty {
                    return None;
                }
                let s = &self.slots[i];
                let online = v.phase == PhaseView::Online;
                let connection = match v.transport {
                    TransportKind::Direct => "helper (no ADB)",
                    TransportKind::Legacy => "ADB (adb tcpip)",
                    TransportKind::Tls => "ADB (wireless debugging)",
                };
                let mirror = match &v.stream {
                    StreamView::Live { width, height } => json!([width, height]),
                    _ => Value::Null,
                };
                let link = self.direct[i].as_ref();
                Some(json!({
                    "slot": i + 1,
                    "name": v.title,
                    "state": format!("{:?}", v.phase).to_lowercase(),
                    "model": v.model,
                    "manufacturer": v.manufacturer,
                    "android_sdk": v.sdk,
                    "connection": connection,
                    "adb": online && self.adb_target(i).is_some(),
                    "helper_version": link.map(|l| l.hello.version),
                    "touch_control": link.map(|l| l.hello.input),
                    "screen": self.auto.screen[i].map(|(w, h)| json!([w, h])),
                    "mirror": mirror,
                    "recording": self.io[i].mailbox.is_recording(),
                    "warning": v.warning,
                    "game": self.game_summary(i),
                    "id": s.device_id(),
                }))
            })
            .collect();
        let run = self.run_view();
        json!({
            "phones": list,
            "test_run": run.active.then(|| json!({ "name": run.name, "dir": run.dir, "crashes": run.crashes })),
            "confirm_destructive": self.cfg.automation.confirm_destructive,
        })
    }

    /// Asks the user in the app; resolves to their answer (no on timeout).
    fn confirm(&mut self, title: String, body: String) -> impl Future<Output = bool> + Send + 'static {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        self.auto.confirms.insert(id, tx);
        self.send(UiEvent::Confirm { id, title, body });
        async move { tokio::time::timeout(CONFIRM_TIMEOUT, rx).await.ok().and_then(Result::ok).unwrap_or(false) }
    }

    pub(super) fn on_confirm(&mut self, id: u64, ok: bool) {
        if let Some(tx) = self.auto.confirms.remove(&id) {
            let _ = tx.send(ok);
        }
    }

    pub(super) fn on_frame_saved(&mut self, request: u64, error: Option<String>) {
        if let Some(tx) = self.auto.frames.remove(&request) {
            let _ = tx.send(error.map_or(Ok(()), Err));
        }
    }

    pub(super) fn note_crash(&mut self, slot: SlotId, crash: &CrashView) {
        let Some(i) = self.idx(slot) else { return };
        self.auto.crash_seq += 1;
        let entry = (self.auto.crash_seq, slot, self.title(i), crash.clone());
        self.auto.crashes.push_back(entry);
        if self.auto.crashes.len() > CRASH_LOG {
            self.auto.crashes.pop_front();
        }
    }

    pub(super) fn note_perf(&mut self, i: usize, sample: &PerfSample) {
        self.auto.last_perf[i] = Some((Instant::now(), sample.clone()));
    }

    /// Automation asked for performance numbers recently.
    pub(super) fn control_perf(&self) -> bool {
        self.auto.perf_until.is_some_and(|t| Instant::now() < t)
    }

    fn screenshot_job(&mut self, t: Target, save: bool) -> Job {
        let Some(i) = self.idx(t.slot) else { return Box::pin(async { Err("gone".to_owned()) }) };
        let path = if save {
            let (dir, stem) = self.capture_target(i, "Screenshot", "Screenshots");
            unique(&dir, &stem, "png")
        } else {
            let dir = crate::control::socket_path().with_file_name("shots");
            let _ = std::fs::create_dir_all(&dir);
            dir.join(format!("slot{}.png", t.slot))
        };
        let screen = self.auto.screen[i];
        match t.adb.clone() {
            Some((serial, ..)) => {
                let adb = self.adb.clone();
                Box::pin(async move {
                    screencap(&adb, &serial, &path).await?;
                    shot_result(&t, &path, None)
                })
            }
            None => {
                let id = self.next_id();
                let (tx, rx) = oneshot::channel();
                self.auto.frames.insert(id, tx);
                self.send(UiEvent::SaveFrame { slot: t.slot, path: path.clone(), request: Some(id) });
                Box::pin(async move {
                    match tokio::time::timeout(Duration::from_secs(10), rx).await {
                        Ok(Ok(Ok(()))) => shot_result(&t, &path, screen),
                        Ok(Ok(Err(e))) => Err(e),
                        _ => Err("the app did not save the frame (is the phone mirroring?)".into()),
                    }
                })
            }
        }
    }
}

/// `{path, width, height}` of a saved screenshot; `screen` is the phone's
/// size when the image is the (smaller) mirror frame.
fn shot_result(t: &Target, path: &std::path::Path, screen: Option<(u32, u32)>) -> Result<Value, String> {
    let (w, h) = png_size(path).ok_or_else(|| format!("{} is not a PNG", path.display()))?;
    let (sw, sh) = screen.unwrap_or((w, h));
    Ok(json!({ "phone": t.name, "slot": t.slot, "path": path, "width": w, "height": h, "screen": [sw, sh] }))
}

fn png_size(path: &std::path::Path) -> Option<(u32, u32)> {
    use std::io::Read;
    let mut b = [0u8; 24];
    std::fs::File::open(path).ok()?.read_exact(&mut b).ok()?;
    b.starts_with(b"\x89PNG").then(|| (u32::from_be_bytes([b[16], b[17], b[18], b[19]]), u32::from_be_bytes([b[20], b[21], b[22], b[23]])))
}

pub(super) fn names_of(ts: &[Target]) -> Vec<(SlotId, String)> {
    ts.iter().map(|t| (t.slot, t.name.clone())).collect()
}

/// Runs the jobs at once; one phone's answer, or a list for several.
pub(super) async fn fan(names: Vec<(SlotId, String)>, jobs: Vec<Job>) -> Result<Value, String> {
    let n = jobs.len();
    if n == 0 {
        return Err("No phone to do it on".into());
    }
    let mut handles = Vec::with_capacity(n);
    for j in jobs {
        handles.push(tokio::spawn(j));
    }
    if n == 1 {
        return handles.pop().expect("one").await.unwrap_or_else(|e| Err(e.to_string()));
    }
    let mut out = Vec::with_capacity(n);
    for (h, (slot, phone)) in handles.into_iter().zip(names) {
        out.push(match h.await {
            Ok(Ok(v)) => json!({ "phone": phone, "slot": slot, "result": v }),
            Ok(Err(e)) => json!({ "phone": phone, "slot": slot, "error": e }),
            Err(e) => json!({ "phone": phone, "slot": slot, "error": e.to_string() }),
        });
    }
    Ok(json!(out))
}

pub(super) fn spawn_fan(names: Vec<(SlotId, String)>, jobs: Vec<Job>, reply: Reply) {
    tokio::spawn(async move {
        let _ = reply.send(fan(names, jobs).await);
    });
}

/// One decimal.
fn round(v: Option<f32>) -> Option<f64> {
    v.map(|v| (f64::from(v) * 10.0).round() / 10.0)
}

fn num(p: &Value, k: &str) -> Option<f64> {
    p.get(k).and_then(Value::as_f64)
}

fn no_adb(t: &Target, what: &str) -> String {
    format!("{} is connected without ADB: {what} need ADB (wireless debugging)", t.name)
}

/// The end of long output.
fn tail(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        return s.to_owned();
    }
    let mut at = s.len() - MAX_OUTPUT;
    while !s.is_char_boundary(at) {
        at += 1;
    }
    format!("…({} bytes cut)\n{}", at, &s[at..])
}

fn keycode(v: &Value) -> Option<u32> {
    if let Some(n) = v.as_u64() {
        return Some(n as u32);
    }
    let s = v.as_str()?.trim().to_lowercase().replace([' ', '-'], "_");
    let s = s.strip_prefix("keycode_").unwrap_or(&s);
    Some(match s {
        "back" => 4,
        "home" => 3,
        "enter" | "return" => 66,
        "recents" | "app_switch" | "overview" => 187,
        "power" => 26,
        "delete" | "del" | "backspace" => 67,
        "forward_delete" => 112,
        "tab" => 61,
        "escape" | "esc" => 111,
        "menu" => 82,
        "search" => 84,
        "up" | "dpad_up" => 19,
        "down" | "dpad_down" => 20,
        "left" | "dpad_left" => 21,
        "right" | "dpad_right" => 22,
        "center" | "dpad_center" => 23,
        "volume_up" => 24,
        "volume_down" => 25,
        "mute" | "volume_mute" => 164,
        "wakeup" | "wake" => 224,
        "sleep" => 223,
        "space" => 62,
        "media_play_pause" | "play_pause" => 85,
        "camera" => 27,
        n => n.parse().ok()?,
    })
}

// ---- the helper -----------------------------------------------------------------

/// An automation op on the phone's helper: over its ADB socket, or the link.
async fn dev(ctx: &Ctx, t: &Target, req: Value) -> Result<Value, DevErr> {
    let v = match (&t.adb, &t.link) {
        (Some((serial, v2, _)), _) => dev_adb(ctx, serial, *v2, &req).await?,
        (None, Some((cmds, version))) => {
            if *version < ac_direct::HELPER_V7 {
                return Err(DevErr::Unavailable(format!(
                    "{} has Wireless Debugging Keeper older than 0.7: update it (the app offers it on the phone's tile)",
                    t.name
                )));
            }
            let (tx, rx) = oneshot::channel();
            cmds.send(LinkCmd::Dev { req, reply: tx }).map_err(|_| DevErr::Unavailable("the phone disconnected".into()))?;
            rx.await.map_err(|_| DevErr::Unavailable("the phone disconnected".into()))?.map_err(DevErr::Unavailable)?
        }
        (None, None) => return Err(DevErr::Unavailable("the phone disconnected".into())),
    };
    match v.get("error").and_then(Value::as_str) {
        Some(e) if v.get("need_input").and_then(Value::as_bool).unwrap_or(false) => Err(DevErr::Unavailable(format!(
            "{e}. On {}: Settings › Accessibility › Wireless Debugging Keeper, switch it on",
            t.name
        ))),
        Some(e) => Err(DevErr::Failed(e.to_owned())),
        None => {
            let mut v = v;
            if let Some(o) = v.as_object_mut() {
                o.remove("op");
            }
            Ok(v)
        }
    }
}

/// Over ADB: the helper's local socket (it checks the caller is adb's shell).
/// Switches its touch control on once if the socket is not there.
async fn dev_adb(ctx: &Ctx, serial: &str, v2: bool, req: &Value) -> Result<Value, DevErr> {
    let once = |ctx: &Ctx| {
        let (adb, req) = (ctx.adb.clone(), req.clone());
        let serial = serial.to_owned();
        async move {
            let mut s = adb.open_service(&serial, DEV_SOCKET).await.map_err(|e| e.to_string())?;
            tokio::time::timeout(Duration::from_secs(20), async {
                ac_direct::write_json(&mut s, &req).await?;
                ac_direct::read_json(&mut s).await
            })
            .await
            .map_err(|_| "the helper did not answer".to_owned())?
            .map_err(|e| e.to_string())
        }
    };
    match once(ctx).await {
        Ok(v) => Ok(v),
        Err(first) => {
            let fresh = ctx.enabled.lock().unwrap().insert(serial.to_owned());
            if !fresh || !enable_helper_input(ctx, serial, v2).await {
                return Err(DevErr::Unavailable(format!("no helper automation socket ({first})")));
            }
            for _ in 0..10 {
                tokio::time::sleep(Duration::from_millis(300)).await;
                if let Ok(v) = once(ctx).await {
                    return Ok(v);
                }
            }
            Err(DevErr::Unavailable("the helper's touch control did not start".into()))
        }
    }
}

/// Switches the helper's accessibility service on over ADB, when helper
/// 0.7+ is installed. True when it did.
async fn enable_helper_input(ctx: &Ctx, serial: &str, v2: bool) -> bool {
    const SVC: &str = "org.androidcontroller.keeper/org.androidcontroller.keeper.InputService";
    let cmd = format!(
        "v=$(dumpsys package org.androidcontroller.keeper | grep -m 1 -o 'versionCode=[0-9]*' | cut -d= -f2); \
         [ \"${{v:-0}}\" -ge {} ] || {{ echo @old; exit 0; }}; \
         l=$(settings get secure enabled_accessibility_services); \
         case \"$l\" in *{SVC}*) echo @already;; null|'') settings put secure enabled_accessibility_services {SVC} && echo @on;; \
         *) settings put secure enabled_accessibility_services \"$l:{SVC}\" && echo @on;; esac; \
         settings put secure accessibility_enabled 1",
        ac_direct::HELPER_V7
    );
    match ctx.adb.shell(serial, &cmd, v2).await {
        Ok(o) if o.stdout.contains("@on") => {
            tracing::info!(serial, "switched on the helper's touch control for automation");
            true
        }
        Ok(o) => {
            tracing::debug!(serial, "helper touch control: {}", o.stdout.trim());
            false
        }
        Err(_) => false,
    }
}

// ---- reading the screen -------------------------------------------------------------

/// The screen's elements: from the helper, else (ADB) `uiautomator dump`.
async fn tree(ctx: &Ctx, t: &Target) -> Result<(Vec<Node>, &'static str, Option<String>), String> {
    let helper_err = match dev(ctx, t, json!({ "op": "tree" })).await {
        Ok(v) => {
            let nodes = v.get("nodes").and_then(Value::as_array).map(|a| a.iter().filter_map(Node::from_json).collect()).unwrap_or_default();
            let pkg = v.get("pkg").and_then(Value::as_str).map(str::to_owned);
            return Ok((nodes, "helper", pkg));
        }
        Err(DevErr::Failed(e)) | Err(DevErr::Unavailable(e)) => e,
    };
    let Some((serial, v2, _)) = &t.adb else { return Err(helper_err) };
    let cmd = format!("uiautomator dump {UIAUTOMATOR_XML} >/dev/null 2>&1; cat {UIAUTOMATOR_XML}; rm -f {UIAUTOMATOR_XML}");
    let out = tokio::time::timeout(Duration::from_secs(20), ctx.adb.shell(serial, &cmd, *v2))
        .await
        .map_err(|_| "uiautomator timed out (the screen may never be idle: try turning animations off)".to_owned())?
        .map_err(|e| e.to_string())?;
    if !out.stdout.contains("<hierarchy") {
        return Err(format!("uiautomator could not read the screen: {}", out.stdout.trim()));
    }
    let nodes = uitree::parse_uiautomator(&out.stdout);
    let pkg = nodes
        .iter()
        .find(|n| !n.pkg.is_empty() && n.pkg != "com.android.systemui")
        .or_else(|| nodes.iter().find(|n| !n.pkg.is_empty()))
        .map(|n| n.pkg.clone());
    Ok((nodes, "uiautomator", pkg))
}

fn pick<'a>(nodes: &'a [Node], q: &Query) -> Result<&'a Node, String> {
    let hits = uitree::find(nodes, q);
    match hits.get(q.index) {
        Some(n) => Ok(n),
        None if hits.is_empty() => Err(format!("Nothing on screen with {} (use ui_tree to see what is there)", q.describe())),
        None => Err(format!("Only {} elements with {}; index {} is past them", hits.len(), q.describe(), q.index)),
    }
}

async fn wait_for(ctx: &Ctx, t: &Target, q: &Query, gone: bool, limit: Duration) -> Result<Value, String> {
    let start = Instant::now();
    loop {
        let (nodes, ..) = tree(ctx, t).await?;
        let hit = uitree::find(&nodes, q).get(q.index).map(|n| n.line());
        match (hit, gone) {
            (Some(line), false) => return Ok(json!({ "found": line, "after_ms": start.elapsed().as_millis() as u64 })),
            (None, true) => return Ok(json!({ "gone": true, "after_ms": start.elapsed().as_millis() as u64 })),
            _ => {}
        }
        if start.elapsed() >= limit {
            let what = if gone { "still there" } else { "did not appear" };
            return Err(format!("{} {what} within {} ms", q.describe(), limit.as_millis()));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

// ---- input -----------------------------------------------------------------------------

async fn adb_input(ctx: &Ctx, serial: &str, v2: bool, args: &str) -> Result<(), String> {
    let out = ctx.adb.shell(serial, &format!("input {args} 2>&1"), v2).await.map_err(|e| e.to_string())?;
    let text = out.stdout.trim();
    if text.contains("Exception") || text.starts_with("Error") || text.contains("usage:") {
        return Err(format!("input {args}: {}", text.lines().next().unwrap_or(text)));
    }
    Ok(())
}

async fn tap_at(ctx: &Ctx, t: &Target, x: i32, y: i32, hold_ms: u32) -> Result<(), String> {
    match &t.adb {
        Some((serial, v2, _)) if hold_ms == 0 => adb_input(ctx, serial, *v2, &format!("tap {x} {y}")).await,
        Some((serial, v2, _)) => adb_input(ctx, serial, *v2, &format!("swipe {x} {y} {x} {y} {hold_ms}")).await,
        None => dev(ctx, t, json!({ "op": "tap", "x": x, "y": y, "ms": hold_ms })).await.map(drop).map_err(Into::into),
    }
}

async fn swipe(ctx: &Ctx, t: &Target, [x1, y1, x2, y2]: [i32; 4], ms: u32) -> Result<(), String> {
    match &t.adb {
        Some((serial, v2, _)) => adb_input(ctx, serial, *v2, &format!("swipe {x1} {y1} {x2} {y2} {ms}")).await,
        None => dev(ctx, t, json!({ "op": "swipe", "x1": x1, "y1": y1, "x2": x2, "y2": y2, "ms": ms })).await.map(drop).map_err(Into::into),
    }
}

async fn key(ctx: &Ctx, t: &Target, code: u32) -> Result<(), String> {
    match &t.adb {
        Some((serial, v2, _)) => adb_input(ctx, serial, *v2, &format!("keyevent {code}")).await,
        None => dev(ctx, t, json!({ "op": "key", "code": code })).await.map(drop).map_err(Into::into),
    }
}

/// ADB `input text` takes printable ASCII; anything else goes through the
/// mirror's text injection, or the helper.
async fn type_text(ctx: &Ctx, t: &Target, text: &str) -> Result<(), String> {
    if let Some((serial, v2, _)) = &t.adb {
        if text.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
            // `input text` reads %s as a space.
            for chunk in text.as_bytes().chunks(200) {
                let s = std::str::from_utf8(chunk).unwrap_or_default().replace('%', "\\%").replace(' ', "%s");
                adb_input(ctx, serial, *v2, &format!("text {}", ac_adb::quote(&s).map_err(|e| e.to_string())?)).await?;
            }
            return Ok(());
        }
        if t.io.send(ac_scrcpy::ControlMsg::InjectText(text.to_owned())) {
            return Ok(());
        }
    }
    dev(ctx, t, json!({ "op": "text", "text": text })).await.map(drop).map_err(|e| match e {
        DevErr::Unavailable(why) if t.adb.is_some() => format!("This text needs mirroring on or the helper: {why}"),
        e => e.into(),
    })
}

// ---- apps, logs, settings -----------------------------------------------------------------

async fn app(ctx: &Ctx, t: &Target, action: AppAction) -> Result<String, String> {
    if let Some((serial, v2, _)) = &t.adb {
        let out = ctx.adb.shell(serial, &app_cmd(&action), *v2).await.map_err(|e| e.to_string())?;
        return app_result(&action, &format!("{}{}", out.stdout, out.stderr));
    }
    let Some((cmds, version)) = &t.link else { return Err("the phone disconnected".into()) };
    if *version < ac_direct::HELPER_V6 {
        return Err("Update Wireless Debugging Keeper on this phone (0.6 or newer) for app actions".into());
    }
    let (tx, rx) = oneshot::channel();
    cmds.send(LinkCmd::App { action, reply: Some(tx) }).map_err(|_| "the phone disconnected".to_owned())?;
    rx.await.map_err(|_| "the phone disconnected".to_owned())?
}

async fn packages(ctx: &Ctx, t: &Target) -> Result<Vec<String>, String> {
    if let Some((serial, v2, _)) = &t.adb {
        let out = ctx.adb.shell(serial, "pm list packages -3", *v2).await.map_err(|e| e.to_string())?;
        return Ok(parse_packages(&out.stdout));
    }
    let Some((cmds, _)) = &t.link else { return Err("the phone disconnected".into()) };
    let (tx, rx) = oneshot::channel();
    cmds.send(LinkCmd::Packages { reply: Some(tx) }).map_err(|_| "the phone disconnected".to_owned())?;
    rx.await.map_err(|_| "the phone disconnected".to_owned())?
}

async fn logcat(ctx: &Ctx, t: &Target, lines: u64, pkg: Option<&str>, grep: Option<&str>) -> Result<Value, String> {
    let (serial, v2, _) = t.adb.clone().ok_or_else(|| no_adb(t, "phone logs"))?;
    let mut cmd = String::new();
    if let Some(p) = pkg {
        let q = ac_adb::quote(p).map_err(|e| e.to_string())?;
        cmd.push_str(&format!("p=$(pidof -s {q} 2>/dev/null || pidof {q} 2>/dev/null); [ -n \"$p\" ] || {{ echo @notrunning; exit 0; }}; "));
        cmd.push_str(&format!("logcat -d -v time -t {lines} --pid=$p 2>&1"));
    } else {
        cmd.push_str(&format!("logcat -d -v time -t {lines} 2>&1"));
    }
    let out = tokio::time::timeout(Duration::from_secs(30), ctx.adb.shell(&serial, &cmd, v2))
        .await
        .map_err(|_| "logcat timed out".to_owned())?
        .map_err(|e| e.to_string())?;
    if out.stdout.trim() == "@notrunning" {
        return Err(format!("{} is not running (its log is per process; launch it first, or leave out package)", pkg.unwrap_or_default()));
    }
    let text: String = match grep {
        Some(g) => out.stdout.lines().filter(|l| l.to_lowercase().contains(g)).map(|l| format!("{l}\n")).collect(),
        None => out.stdout,
    };
    Ok(json!(tail(&text)))
}

async fn settings(ctx: &Ctx, t: &Target, anim: Option<bool>, dark: Option<bool>, font: Option<f64>) -> Result<Value, String> {
    let Some((serial, v2, _)) = &t.adb else {
        if dark.is_some() || font.is_some() {
            return Err(no_adb(t, "dark mode and font size"));
        }
        let v = dev(ctx, t, json!({ "op": "settings", "animations": anim })).await?;
        return Ok(v);
    };
    let mut cmd = String::new();
    if let Some(on) = anim {
        let scale = if on { "1" } else { "0" };
        for k in ["window_animation_scale", "transition_animation_scale", "animator_duration_scale"] {
            cmd.push_str(&format!("settings put global {k} {scale}; "));
        }
    }
    if let Some(d) = dark {
        cmd.push_str(&format!("cmd uimode night {}; ", if d { "yes" } else { "no" }));
    }
    if let Some(f) = font {
        cmd.push_str(&format!("settings put system font_scale {}; ", f.clamp(0.5, 2.0)));
    }
    let out = ctx.adb.shell(serial, &format!("{cmd}echo @done"), *v2).await.map_err(|e| e.to_string())?;
    if !out.stdout.contains("@done") {
        return Err(out.stdout.trim().to_owned());
    }
    Ok(json!({ "animations": anim, "dark": dark, "font_scale": font }))
}
