mod pairing;
mod qr;
mod slot_card;
mod window;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use tracing_subscriber::EnvFilter;

const APP_ID: &str = "org.androidcontroller.AndroidController";

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,zbus=warn".into()))
        .init();

    let paths = ac_config::Paths::xdg();
    let (config, config_error) = match paths.as_ref().map(|p| ac_config::load_config(&p.config)) {
        Some(Ok(cfg)) => (cfg, None),
        Some(Err(e)) => {
            tracing::error!("{e}");
            (ac_config::Config::default(), Some(format!("Using default settings: {e}")))
        }
        None => (ac_config::Config::default(), None),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("ac-tokio")
        .enable_all()
        .build()
        .expect("failed to start the tokio runtime");
    let slots = config.general.max_slots.clamp(1, ac_core::MAX_SLOTS as u8);
    let manager = {
        let _guard = runtime.enter();
        ac_device::spawn(ac_device::ManagerOptions::new(config, paths))
    };

    let app = adw::Application::builder().application_id(APP_ID).build();
    let manager = Rc::new(RefCell::new(Some(manager)));
    app.connect_activate(move |app| {
        if let Some(win) = app.active_window() {
            win.present();
            return;
        }
        if let Some(m) = manager.borrow_mut().take() {
            window::build(app, m, slots, config_error.clone());
        }
    });
    let code = app.run();
    runtime.shutdown_timeout(Duration::from_secs(1));
    code
}
