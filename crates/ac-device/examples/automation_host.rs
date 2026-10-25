//! Hosts the device manager without the UI and serves the automation socket
//! at `<socket>`, for checking `android-controller-mcp` against real phones:
//! `automation_host <socket> [output-dir]`. Mirroring is off; confirmation
//! requests are answered "no" (set `AC_CONFIRM=yes` to allow them).
use ac_device::{ManagerOptions, UiCommand, UiEvent};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let socket = args.next().expect("socket path");
    let mut cfg = ac_config::Config::default();
    if let Some(out) = args.next() {
        cfg.testing.output_dir = out;
    }
    let mut opts = ManagerOptions::new(cfg, None);
    opts.discovery = false;
    opts.streaming = false;
    opts.control_socket = Some(socket.into());
    let h = ac_device::spawn(opts);
    let allow = std::env::var("AC_CONFIRM").is_ok_and(|v| v == "yes");
    while let Ok(ev) = h.events.recv().await {
        match ev {
            UiEvent::Slot(v) => println!("slot {}: {:?} {}", v.slot, v.phase, v.title),
            UiEvent::Confirm { id, title, body } => {
                println!("confirm {id}: {title} ({body}) -> {}", if allow { "yes" } else { "no" });
                h.commands.send(UiCommand::Confirm { id, ok: allow }).await.unwrap();
            }
            UiEvent::SaveFrame { slot, request, .. } => {
                println!("save frame {slot} (no mirror here)");
                if let Some(request) = request {
                    h.commands.send(UiCommand::FrameSaved { request, error: Some("no mirror in automation_host".into()) }).await.unwrap();
                }
            }
            UiEvent::Crash { slot, crash } => println!("crash on {slot}: {} {}", crash.kind.label(), crash.summary),
            UiEvent::Automation { clients } => println!("automation clients: {clients}"),
            UiEvent::Toast { message, .. } => println!("toast: {message}"),
            UiEvent::AppResult { slot, ok, message, .. } => println!("app result {slot}: {ok} {message}"),
            _ => {}
        }
    }
}
