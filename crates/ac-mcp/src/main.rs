//! `android-controller-mcp`: an MCP server (stdio) that lets Claude drive the
//! phones connected to a running Android Controller: see what is on screen,
//! tap and type, install and launch apps, read crashes, logs and performance,
//! and talk to games built with the debug add-on (ACaddon.md).
//!
//! It is a thin translator: every tool is a request on the app's automation
//! socket (`$XDG_RUNTIME_DIR/android-controller/control.sock`, JSON lines).
//! Cursor: `~/.cursor/mcp.json` →
//! `{"mcpServers": {"android-controller": {"command": "android-controller-mcp"}}}`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};

const PROTOCOL: &str = "2025-06-18";
/// Screenshots are scaled to fit this (longest side, pixels) by default.
const SHOT_MAX: u32 = 1024;

const INSTRUCTIONS: &str = "\
Android Controller drives the real Android phones connected to it on this PC.\n\
- Start with `phones` to see which phones are connected and how (ADB or helper).\n\
- Every tool takes `phone`: a name, slot number, or \"all\" (where it makes sense). Leave it out when only one phone is connected.\n\
- To see the screen: `ui_tree` (fast, text, with tap centres) for normal apps; `screenshot` for games and anything drawn as pixels.\n\
- Prefer `tap` with text/id/desc over coordinates; coordinates are always the phone's screen pixels (a screenshot says how to scale).\n\
- After an action, use `wait_for` instead of sleeping, then check `crashes` (and `game_logs` for games).\n\
- Games built with the debug add-on (ACaddon.md) answer `game_info`, `game_state` and `game_command` (load a level, give coins...).\n\
- Clearing app data or uninstalling may wait for the user to allow it in the app window.";

fn main() {
    let app = Arc::new(App::default());
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                send(&out, &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": e.to_string() } }));
                continue;
            }
        };
        let (app, out) = (app.clone(), out.clone());
        // Calls may be slow (installs, waits): answer them in their own thread.
        std::thread::spawn(move || {
            if let Some(reply) = handle(&app, &msg) {
                send(&out, &reply);
            }
        });
    }
}

fn send(out: &Mutex<std::io::Stdout>, v: &Value) {
    let mut o = out.lock().unwrap();
    let _ = writeln!(o, "{v}");
    let _ = o.flush();
}

/// One JSON-RPC message; `None` for notifications.
fn handle(app: &App, msg: &Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL);
            Ok(json!({
                "protocolVersion": asked,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "android-controller", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools().iter().map(Tool::listing).collect::<Vec<_>>() })),
        "tools/call" => Ok(call_tool(app, &params)),
        _ => Err((-32601, format!("unknown method {method}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    })
}

fn call_tool(app: &App, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let args = params.get("arguments").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
    let Some(tool) = tools().into_iter().find(|t| t.name == name) else {
        return error_content(format!("unknown tool {name}"));
    };
    let (method, req) = (tool.request)(&args);
    let limit = timeout_for(method, &args);
    match app.call(method, req, limit) {
        Ok(v) => format_result(name, &args, &v),
        Err(e) => error_content(e),
    }
}

fn timeout_for(method: &str, args: &Value) -> Duration {
    let ms = |k: &str, d: u64| args.get(k).and_then(Value::as_u64).unwrap_or(d);
    Duration::from_millis(match method {
        "install" => 11 * 60_000,
        "app" => 4 * 60_000, // may wait for the user to allow it
        "wait_for" => ms("timeout_ms", 10_000) + 30_000,
        "shell" => ms("timeout_ms", 30_000) + 10_000,
        "game_command" | "game_state" => ms("timeout_ms", 10_000) + 10_000,
        _ => 60_000,
    })
}

fn text_content(text: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }] })
}

fn error_content(text: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }], "isError": true })
}

/// Tool answers as text (and images for screenshots).
fn format_result(tool: &str, args: &Value, v: &Value) -> Value {
    match tool {
        "screenshot" => {
            let max = args.get("max_size").and_then(Value::as_u64).map_or(SHOT_MAX, |m| m.clamp(160, 4096) as u32);
            let shots: Vec<&Value> = match v {
                Value::Array(a) => a.iter().collect(),
                one => vec![one],
            };
            let mut content = Vec::new();
            for s in shots {
                let s = s.get("result").unwrap_or(s);
                if let Some(e) = s.get("error").and_then(Value::as_str) {
                    content.push(json!({ "type": "text", "text": format!("error: {e}") }));
                    continue;
                }
                match screenshot_content(s, max) {
                    Ok(mut c) => content.append(&mut c),
                    Err(e) => content.push(json!({ "type": "text", "text": e })),
                }
            }
            json!({ "content": content })
        }
        "ui_tree" => match v {
            Value::Object(o) => {
                let head = format!(
                    "{} (from {}): one element per line, @x,y is its tap centre in phone pixels\n",
                    o.get("package").and_then(Value::as_str).unwrap_or("screen"),
                    o.get("source").and_then(Value::as_str).unwrap_or("?")
                );
                text_content(head + o.get("text").and_then(Value::as_str).unwrap_or_default())
            }
            other => text_content(pretty(other)),
        },
        "logcat" | "game_logs" => match v {
            Value::String(s) if s.is_empty() => text_content("(no matching lines)"),
            Value::String(s) => text_content(s.clone()),
            Value::Object(o) if o.contains_key("text") => {
                let text = o.get("text").and_then(Value::as_str).unwrap_or_default();
                let mut out = if text.is_empty() { "(no new lines)\n".to_owned() } else { text.to_owned() };
                out.push_str(&format!("(next: {})", o.get("next").and_then(Value::as_u64).unwrap_or(0)));
                if o.get("connected").and_then(Value::as_bool) == Some(false) {
                    out.push_str(" — no game with the add-on is connected right now");
                }
                text_content(out)
            }
            other => text_content(pretty(other)),
        },
        _ => match v {
            Value::String(s) => text_content(s.clone()),
            other => text_content(pretty(other)),
        },
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// The screenshot, scaled to fit `max` and sent as JPEG, plus how its
/// pixels map to the phone's.
fn screenshot_content(s: &Value, max: u32) -> Result<Vec<Value>, String> {
    let path = s.get("path").and_then(Value::as_str).ok_or("no screenshot path")?;
    let img = image::open(path).map_err(|e| format!("reading {path}: {e}"))?;
    let (w, h) = (img.width(), img.height());
    let screen = s.get("screen").and_then(Value::as_array);
    let sw = screen.and_then(|a| a.first()?.as_u64()).map_or(w, |x| x as u32);
    let sh = screen.and_then(|a| a.get(1)?.as_u64()).map_or(h, |x| x as u32);
    let img = if w.max(h) > max { img.resize(max, max, image::imageops::FilterType::Triangle) } else { img };
    let mut jpeg = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 80);
    img.to_rgb8().write_with_encoder(enc).map_err(|e| e.to_string())?;
    let scale = sw as f64 / img.width() as f64;
    let phone = s.get("phone").and_then(Value::as_str).unwrap_or("phone");
    let mut note = format!(
        "{phone}: image {}x{}, phone screen {sw}x{sh}. Tap coordinates are phone pixels = image pixels × {scale:.3}.",
        img.width(),
        img.height()
    );
    if s.get("width").and_then(Value::as_u64).is_some_and(|x| x as u32 != sw) {
        note.push_str(" (Taken from the mirror: the phone has no ADB.)");
    }
    if let Some(p) = s.get("path").and_then(Value::as_str).filter(|p| !p.contains("/shots/")) {
        note.push_str(&format!(" Saved at {p}"));
    }
    Ok(vec![
        json!({ "type": "image", "data": base64::engine::general_purpose::STANDARD.encode(&jpeg), "mimeType": "image/jpeg" }),
        json!({ "type": "text", "text": note }),
    ])
}

// ---- the app's control socket -----------------------------------------------------------

type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>;

#[derive(Default)]
struct App {
    conn: Mutex<Option<Conn>>,
    next: AtomicU64,
}

struct Conn {
    writer: UnixStream,
    pending: Pending,
    alive: Arc<std::sync::atomic::AtomicBool>,
}

fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("ANDROID_CONTROLLER_SOCKET") {
        return p.into();
    }
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("android-controller").join("control.sock"),
        None => {
            // SAFETY: getuid has no failure mode.
            let uid = unsafe { getuid() };
            PathBuf::from(format!("/tmp/android-controller-{uid}")).join("control.sock")
        }
    }
}

unsafe extern "C" {
    fn getuid() -> u32;
}

impl App {
    fn connect(&self) -> Result<(), String> {
        let mut conn = self.conn.lock().unwrap();
        if conn.as_ref().is_some_and(|c| c.alive.load(Ordering::Relaxed)) {
            return Ok(());
        }
        let path = socket_path();
        let s = UnixStream::connect(&path).map_err(|e| {
            format!(
                "Android Controller is not running, or automation is off (Preferences › Testing › Automation). ({}: {e})",
                path.display()
            )
        })?;
        let reader = s.try_clone().map_err(|e| e.to_string())?;
        let pending: Pending = Arc::default();
        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (p, a) = (pending.clone(), alive.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let Some(id) = v.get("id").and_then(Value::as_u64) else { continue };
                let r = match v.get("error") {
                    Some(e) if !e.is_null() => Err(e.as_str().map_or_else(|| e.to_string(), str::to_owned)),
                    _ => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                };
                if let Some(tx) = p.lock().unwrap().remove(&id) {
                    let _ = tx.send(r);
                }
            }
            a.store(false, Ordering::Relaxed);
            for (_, tx) in p.lock().unwrap().drain() {
                let _ = tx.send(Err("Android Controller closed the connection".into()));
            }
        });
        *conn = Some(Conn { writer: s, pending, alive });
        Ok(())
    }

    fn call(&self, method: &str, params: Value, limit: Duration) -> Result<Value, String> {
        self.connect()?;
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::channel();
        {
            let mut guard = self.conn.lock().unwrap();
            let conn = guard.as_mut().ok_or("not connected")?;
            conn.pending.lock().unwrap().insert(id, tx);
            let line = json!({ "id": id, "method": method, "params": params }).to_string() + "\n";
            if let Err(e) = conn.writer.write_all(line.as_bytes()) {
                conn.alive.store(false, Ordering::Relaxed);
                return Err(format!("Android Controller went away: {e}"));
            }
        }
        rx.recv_timeout(limit).map_err(|_| format!("no answer from Android Controller within {} s", limit.as_secs()))?
    }
}

// ---- tools -----------------------------------------------------------------------------

struct Tool {
    name: &'static str,
    description: &'static str,
    schema: Value,
    /// The socket method and its params.
    request: fn(&Value) -> (&'static str, Value),
}

impl Tool {
    fn listing(&self) -> Value {
        json!({ "name": self.name, "description": self.description, "inputSchema": self.schema })
    }
}

fn phone_prop(all: bool) -> Value {
    let d = if all {
        "Phone name, slot number, or \"all\". Optional when one phone is connected."
    } else {
        "Phone name or slot number. Optional when one phone is connected."
    };
    json!({ "type": ["string", "integer"], "description": d })
}

/// `{"type": "object", properties}` with `phone` added.
fn schema(all: bool, props: Value, required: &[&str]) -> Value {
    let mut p = props.as_object().cloned().unwrap_or_default();
    p.insert("phone".into(), phone_prop(all));
    json!({ "type": "object", "properties": p, "required": required })
}

fn element_props(extra: Value) -> Value {
    let mut p = json!({
        "text": { "type": "string", "description": "Visible text (exact or part, any case; also matches an icon's description)" },
        "id": { "type": "string", "description": "Resource id, e.g. btn_play or com.example:id/btn_play" },
        "desc": { "type": "string", "description": "Content description (accessibility label)" },
        "index": { "type": "integer", "description": "Which match when several fit (0 = best/first)" },
    });
    if let (Some(p), Some(e)) = (p.as_object_mut(), extra.as_object()) {
        p.extend(e.clone());
    }
    p
}

fn tools() -> Vec<Tool> {
    macro_rules! pass {
        ($m:literal) => {
            |a: &Value| ($m, a.clone())
        };
    }
    vec![
        Tool {
            name: "phones",
            description: "List the phones in Android Controller: name, slot, connection (ADB or helper), Android version, screen size, mirroring, whether a game with the debug add-on is running, and the test run.",
            schema: json!({ "type": "object", "properties": {} }),
            request: pass!("phones"),
        },
        Tool {
            name: "screenshot",
            description: "See a phone's screen as an image. Use it for games and custom-drawn UI; for normal apps ui_tree is faster and gives exact tap points. The answer says how image pixels map to phone pixels.",
            schema: schema(
                true,
                json!({
                    "save": { "type": "boolean", "description": "Also keep a full-resolution PNG (in the test run's folder during a run)" },
                    "max_size": { "type": "integer", "description": "Longest side of the returned image (default 1024)" },
                }),
                &[],
            ),
            request: pass!("screenshot"),
        },
        Tool {
            name: "tap",
            description: "Tap the screen: at x,y (phone pixels), or on the element with this text / id / desc (its centre). long=true for a long press.",
            schema: schema(
                true,
                element_props(json!({
                    "x": { "type": "number" }, "y": { "type": "number" },
                    "long": { "type": "boolean", "description": "Long press (0.8 s)" },
                    "duration_ms": { "type": "integer", "description": "Hold this long" },
                })),
                &[],
            ),
            request: pass!("tap"),
        },
        Tool {
            name: "swipe",
            description: "Swipe from x1,y1 to x2,y2 (phone pixels). Scroll a list down: swipe from lower to higher on the screen (y1 > y2).",
            schema: schema(
                true,
                json!({
                    "x1": { "type": "number" }, "y1": { "type": "number" }, "x2": { "type": "number" }, "y2": { "type": "number" },
                    "duration_ms": { "type": "integer", "description": "Default 300; longer is a slow drag" },
                }),
                &["x1", "y1", "x2", "y2"],
            ),
            request: pass!("swipe"),
        },
        Tool {
            name: "type_text",
            description: "Type text into the focused field (tap the field first). enter=true presses Enter after.",
            schema: schema(true, json!({ "text": { "type": "string" }, "enter": { "type": "boolean" } }), &["text"]),
            request: |a| ("type", a.clone()),
        },
        Tool {
            name: "press_key",
            description: "Press a key: back, home, enter, recents, power, delete, tab, escape, up, down, left, right, volume_up, volume_down, wakeup, or an Android key code.",
            schema: schema(true, json!({ "key": { "type": ["string", "integer"] } }), &["key"]),
            request: |a| ("key", a.clone()),
        },
        Tool {
            name: "ui_tree",
            description: "What is on screen, as text: one element per line with its text, id, tap centre @x,y, size and flags (click, scroll, edit, checked...). Fast with the helper app (0.7+), else via uiautomator (1-3 s). Games draw pixels: use screenshot or the game add-on there.",
            schema: schema(false, json!({ "all": { "type": "boolean", "description": "Include layout-only elements too" } }), &[]),
            request: pass!("ui_tree"),
        },
        Tool {
            name: "find_element",
            description: "Find elements on screen by text / id / desc; returns each match's line (with its tap centre), best first.",
            schema: schema(false, element_props(json!({})), &[]),
            request: |a| ("find", a.clone()),
        },
        Tool {
            name: "wait_for",
            description: "Wait until an element with this text / id / desc is on screen (or gone=true: until it is not). Use it after taps instead of sleeping.",
            schema: schema(
                false,
                element_props(json!({
                    "gone": { "type": "boolean" },
                    "timeout_ms": { "type": "integer", "description": "Default 10000" },
                })),
                &[],
            ),
            request: pass!("wait_for"),
        },
        Tool {
            name: "screen_events",
            description: "Popups, dialogs, toasts and screen changes the helper app (0.7+) saw, oldest first. Pass since = the last answer's `next` to get only new ones.",
            schema: schema(true, json!({ "since": { "type": "integer" } }), &[]),
            request: |a| ("events", a.clone()),
        },
        Tool {
            name: "auto_allow_permissions",
            description: "With the helper app (0.7+): tap \"Allow\" on Android permission prompts automatically (on=false stops).",
            schema: schema(true, json!({ "on": { "type": "boolean" } }), &[]),
            request: |a| ("auto_allow", a.clone()),
        },
        Tool {
            name: "install_apk",
            description: "Install an APK from this PC on the phone(s) (replaces an older version). Waits until installed.",
            schema: schema(true, json!({ "path": { "type": "string", "description": "Absolute path of the .apk on this PC" } }), &["path"]),
            request: |a| ("install", a.clone()),
        },
        Tool {
            name: "app",
            description: "launch, stop, restart, clear_data or uninstall an app by package name. clear_data and uninstall may wait for the user to allow it on the PC.",
            schema: schema(
                true,
                json!({
                    "action": { "type": "string", "enum": ["launch", "stop", "restart", "clear_data", "uninstall"] },
                    "package": { "type": "string" },
                }),
                &["action", "package"],
            ),
            request: pass!("app"),
        },
        Tool {
            name: "list_packages",
            description: "The apps the user installed on the phone (package names).",
            schema: schema(false, json!({}), &[]),
            request: |a| ("packages", a.clone()),
        },
        Tool {
            name: "crashes",
            description: "Crashes, ANRs, native crashes and game exceptions seen on the phones since the app started (ADB phones; games with the add-on too), with stack traces. Pass since = the last answer's `next` for only new ones.",
            schema: json!({ "type": "object", "properties": { "since": { "type": "integer" } } }),
            request: pass!("crashes"),
        },
        Tool {
            name: "perf",
            description: "Frame rate, CPU, memory (the phone's and the app's), temperatures. The first call starts sampling: ask again after a few seconds.",
            schema: schema(true, json!({}), &[]),
            request: pass!("perf"),
        },
        Tool {
            name: "logcat",
            description: "Recent lines of the phone's log (ADB only), optionally only one app's (package, which must be running) and/or lines containing grep.",
            schema: schema(
                false,
                json!({
                    "lines": { "type": "integer", "description": "Default 200" },
                    "package": { "type": "string" },
                    "grep": { "type": "string", "description": "Keep lines containing this (any case)" },
                }),
                &[],
            ),
            request: pass!("logcat"),
        },
        Tool {
            name: "shell",
            description: "Run a shell command on the phone (ADB only), e.g. dumpsys, am, pm, settings.",
            schema: schema(true, json!({ "command": { "type": "string" }, "timeout_ms": { "type": "integer" } }), &["command"]),
            request: pass!("shell"),
        },
        Tool {
            name: "test_run",
            description: "Start or stop a test run: every phone's full log, crashes, performance and screenshots/recordings go into one folder; stop returns its summary path.",
            schema: json!({ "type": "object", "properties": {
                "action": { "type": "string", "enum": ["start", "stop"] },
                "name": { "type": "string" },
            }, "required": ["action"] }),
            request: |a| {
                let stop = a.get("action").and_then(Value::as_str) == Some("stop");
                (if stop { "run_stop" } else { "run_start" }, a.clone())
            },
        },
        Tool {
            name: "record",
            description: "Start (on=true) or stop recording a phone's screen to an MP4 (the phone must be mirroring).",
            schema: schema(true, json!({ "on": { "type": "boolean" } }), &["on"]),
            request: pass!("record"),
        },
        Tool {
            name: "test_settings",
            description: "Change phone settings for testing: animations off (steadier tests), dark mode, font scale. Without ADB only animations.",
            schema: schema(
                true,
                json!({
                    "animations": { "type": "boolean" },
                    "dark": { "type": "boolean" },
                    "font_scale": { "type": "number", "description": "1.0 normal, 1.3 large" },
                }),
                &[],
            ),
            request: |a| ("settings", a.clone()),
        },
        Tool {
            name: "game_info",
            description: "The game running with the debug add-on (ACaddon.md): name, version, engine, and the commands it offers with their arguments.",
            schema: schema(true, json!({}), &[]),
            request: pass!("game_info"),
        },
        Tool {
            name: "game_state",
            description: "The game's own state as it reports it (scene, level, player, score, coins... whatever the game exposes).",
            schema: schema(true, json!({ "timeout_ms": { "type": "integer" } }), &[]),
            request: pass!("game_state"),
        },
        Tool {
            name: "game_command",
            description: "Run one of the game's debug commands (see game_info), e.g. name=load_level args={\"level\": 3}.",
            schema: schema(
                false,
                json!({
                    "name": { "type": "string" },
                    "args": { "type": "object" },
                    "timeout_ms": { "type": "integer" },
                }),
                &["name"],
            ),
            request: pass!("game_command"),
        },
        Tool {
            name: "game_logs",
            description: "The game's log lines and events from the add-on (with exceptions and their stacks). Pass since = the last answer's `next` for only new ones; level = error|warn|info|debug for at least that level.",
            schema: schema(true, json!({ "since": { "type": "integer" }, "level": { "type": "string" } }), &[]),
            request: pass!("game_logs"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_tools_with_object_schemas() {
        let t = tools();
        let mut names: Vec<_> = t.iter().map(|t| t.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), t.len(), "unique names");
        for tool in &t {
            assert_eq!(tool.schema["type"], "object", "{}", tool.name);
            assert!(tool.description.len() > 20);
        }
    }

    #[test]
    fn answers_initialize_and_unknown_methods() {
        let app = App::default();
        let r = handle(&app, &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } })).unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert!(r["result"]["instructions"].as_str().unwrap().contains("phones"));
        assert!(handle(&app, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
        let r = handle(&app, &json!({ "jsonrpc": "2.0", "id": 2, "method": "nope" })).unwrap();
        assert_eq!(r["error"]["code"], -32601);
        let r = handle(&app, &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" })).unwrap();
        assert!(r["result"]["tools"].as_array().unwrap().len() > 20);
    }

    #[test]
    fn calls_through_the_socket() {
        // A fake app on a socket of our own.
        let dir = std::env::temp_dir().join(format!("ac-mcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("control.sock");
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let png = dir.join("shot.png");
        image::RgbImage::from_pixel(1080, 2400, image::Rgb([10, 20, 30])).save(&png).unwrap();
        let png_s = png.to_string_lossy().into_owned();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            for line in BufReader::new(s).lines() {
                let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
                let r = match v["method"].as_str().unwrap() {
                    "screenshot" => json!({ "id": v["id"], "result": { "phone": "Moto", "path": png_s, "width": 1080, "height": 2400, "screen": [1080, 2400] } }),
                    "tap" => json!({ "id": v["id"], "result": { "x": v["params"]["x"], "y": 5 } }),
                    _ => json!({ "id": v["id"], "error": "no such phone" }),
                };
                writeln!(w, "{r}").unwrap();
            }
        });
        // SAFETY: tests in this binary that read the variable all agree on it.
        unsafe { std::env::set_var("ANDROID_CONTROLLER_SOCKET", &path) };
        let app = App::default();
        let r = call_tool(&app, &json!({ "name": "tap", "arguments": { "x": 7, "y": 5 } }));
        assert_eq!(r["content"][0]["text"].as_str().unwrap().replace(char::is_whitespace, ""), r#"{"x":7,"y":5}"#);
        let r = call_tool(&app, &json!({ "name": "shell", "arguments": { "command": "ls" } }));
        assert_eq!(r["isError"], true);
        assert_eq!(r["content"][0]["text"], "no such phone");
        let r = call_tool(&app, &json!({ "name": "screenshot", "arguments": {} }));
        assert_eq!(r["content"][0]["type"], "image");
        let note = r["content"][1]["text"].as_str().unwrap();
        assert!(note.contains("image 461x1024") && note.contains("× 2.343"), "{note}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
