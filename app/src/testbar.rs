//! The toolbar under the header: adding phones, and app testing on every
//! connected phone at once (Milestone 5). A test run saves each phone's log, crashes and graphs in
//! one folder; the app row installs, launches, stops, clears and
//! uninstalls an app everywhere; captures take screenshots and recordings.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use ac_device::{AppAction, RunView, UiCommand};
use adw::prelude::*;
use gtk::{gio, glib};

use crate::window::App;

/// Builds the action from the package name.
type MakeAction = fn(String) -> AppAction;

pub struct TestBar {
    pub root: gtk::FlowBox,
    run_name: gtk::Entry,
    run_button: gtk::Button,
    run_status: gtk::Label,
    open_run: gtk::Button,
    apk_label: gtk::Label,
    install: gtk::Button,
    apk: RefCell<Option<PathBuf>>,
    package: gtk::Entry,
    packages_list: gtk::Box,
    known: RefCell<BTreeSet<String>>,
    pub record: gtk::ToggleButton,
    /// Set while the record toggle follows the phones, so it sends nothing.
    syncing: Cell<bool>,
    pub graphs: gtk::ToggleButton,
    run: RefCell<RunView>,
    started: Cell<Option<Instant>>,
}

fn button(label: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder().label(label).tooltip_text(tooltip).build()
}

fn content(icon: &str, label: &str) -> adw::ButtonContent {
    adw::ButtonContent::builder().icon_name(icon).label(label).build()
}

fn section(title: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    b.add_css_class("test-section");
    let l = gtk::Label::new(Some(title));
    l.add_css_class("heading");
    l.add_css_class("dim-label");
    b.append(&l);
    b
}

impl TestBar {
    /// `phones`: the Add Device and Nearby buttons, shown first.
    pub fn new(app: &Rc<App>, phones: &[&gtk::Widget]) -> Rc<Self> {
        let weak = Rc::downgrade(app);

        let phones_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        phones_box.add_css_class("test-section");
        for w in phones {
            phones_box.append(*w);
        }

        // Run.
        let run_box = section("Run");
        let run_name = gtk::Entry::builder().placeholder_text("Name (optional)").width_chars(14).build();
        let run_button = button("Start run", "Save each phone's log, crashes and graphs in one folder until you stop");
        run_button.add_css_class("suggested-action");
        let run_status = gtk::Label::new(None);
        run_status.add_css_class("numeric");
        let open_run = gtk::Button::builder().icon_name("folder-open-symbolic").tooltip_text("Open the run's folder").visible(false).build();
        open_run.add_css_class("flat");
        run_box.append(&run_name);
        run_box.append(&run_button);
        run_box.append(&run_status);
        run_box.append(&open_run);

        // App.
        let app_box = section("App");
        let choose = gtk::Button::builder().child(&content("document-open-symbolic", "APK…")).tooltip_text("Choose an APK to install").build();
        let apk_label = gtk::Label::builder().label("No APK").max_width_chars(22).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
        apk_label.add_css_class("dim-label");
        let install = button("Install", "Install the APK on every connected phone");
        install.set_sensitive(false);
        let package = gtk::Entry::builder().placeholder_text("Package, e.g. com.example.game").width_chars(24).build();
        let packages_list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let packages_scroll = gtk::ScrolledWindow::builder()
            .child(&packages_list)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(360)
            .min_content_width(280)
            .build();
        let packages_popover = gtk::Popover::builder().child(&packages_scroll).build();
        let packages_button = gtk::MenuButton::builder()
            .icon_name("view-list-symbolic")
            .tooltip_text("Apps installed on the phones")
            .popover(&packages_popover)
            .build();
        app_box.append(&choose);
        app_box.append(&apk_label);
        app_box.append(&install);
        app_box.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        app_box.append(&package);
        app_box.append(&packages_button);
        let actions: [(&str, &str, MakeAction); 4] = [
            ("Launch", "Open the app on every phone", AppAction::Launch),
            ("Stop", "Force-stop the app on every phone", AppAction::Stop),
            ("Clear data", "Wipe the app's data on every phone", AppAction::ClearData),
            ("Uninstall", "Remove the app from every phone", AppAction::Uninstall),
        ];
        let action_buttons: Vec<(gtk::Button, MakeAction)> =
            actions.iter().map(|(l, t, a)| (button(l, t), *a)).collect();
        for (b, _) in &action_buttons {
            app_box.append(b);
        }

        // Captures.
        let cap_box = section("Capture");
        let shot = gtk::Button::builder().child(&content("camera-photo-symbolic", "Screenshot")).tooltip_text("Screenshot every phone").build();
        let record =
            gtk::ToggleButton::builder().child(&content("media-record-symbolic", "Record")).tooltip_text("Record every phone's screen").build();
        record.add_css_class("record-toggle");
        let graphs = gtk::ToggleButton::builder()
            .child(&content("utilities-system-monitor-symbolic", "Graphs"))
            .tooltip_text("Show frame rate, CPU, memory and temperature on each tile (always on during a run)")
            .build();
        cap_box.append(&shot);
        cap_box.append(&record);
        cap_box.append(&graphs);

        let row = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .column_spacing(18)
            .row_spacing(6)
            .max_children_per_line(4)
            .homogeneous(false)
            .build();
        for b in [&phones_box, &cap_box, &run_box, &app_box] {
            row.append(b);
        }
        row.add_css_class("test-bar");

        let bar = Rc::new(Self {
            root: row,
            run_name,
            run_button,
            run_status,
            open_run,
            apk_label,
            install,
            apk: RefCell::default(),
            package,
            packages_list,
            known: RefCell::default(),
            record,
            syncing: Cell::new(false),
            graphs,
            run: RefCell::new(RunView { active: false, name: String::new(), dir: PathBuf::new(), crashes: 0 }),
            started: Cell::new(None),
        });

        let w = weak.clone();
        let b = Rc::downgrade(&bar);
        bar.run_button.connect_clicked(move |_| {
            let (Some(app), Some(bar)) = (w.upgrade(), b.upgrade()) else { return };
            if bar.run.borrow().active {
                app.send(UiCommand::StopRun);
            } else {
                app.start_run(bar.run_name.text().trim());
            }
        });
        let b = Rc::downgrade(&bar);
        bar.run_name.connect_activate(move |_| {
            if let Some(bar) = b.upgrade()
                && !bar.run.borrow().active
            {
                bar.run_button.emit_clicked();
            }
        });
        let w = weak.clone();
        let b = Rc::downgrade(&bar);
        bar.open_run.connect_clicked(move |_| {
            if let (Some(app), Some(bar)) = (w.upgrade(), b.upgrade()) {
                app.open_folder(&bar.run.borrow().dir);
            }
        });
        let w = weak.clone();
        let b = Rc::downgrade(&bar);
        choose.connect_clicked(move |_| {
            if let (Some(app), Some(bar)) = (w.upgrade(), b.upgrade()) {
                bar.choose_apk(&app);
            }
        });
        let w = weak.clone();
        let b = Rc::downgrade(&bar);
        bar.install.connect_clicked(move |_| {
            let (Some(app), Some(bar)) = (w.upgrade(), b.upgrade()) else { return };
            let Some(path) = bar.apk.borrow().clone() else { return };
            let slots = app.online_slots();
            if slots.is_empty() {
                return app.toast("No phone is connected");
            }
            app.clear_results();
            app.send(UiCommand::SendFiles { slots, paths: vec![path], install_apks: true });
        });
        let w = weak.clone();
        bar.package.connect_changed(move |e| {
            if let Some(app) = w.upgrade() {
                let p = e.text().trim().to_owned();
                app.send(UiCommand::SetTestApp { package: (!p.is_empty()).then_some(p) });
            }
        });
        let w = weak.clone();
        let b = Rc::downgrade(&bar);
        packages_popover.connect_show(move |_| {
            let (Some(app), Some(bar)) = (w.upgrade(), b.upgrade()) else { return };
            bar.render_packages();
            for slot in app.online_slots() {
                app.send(UiCommand::ListPackages { slot });
            }
        });
        for (btn, make) in action_buttons {
            let w = weak.clone();
            let b = Rc::downgrade(&bar);
            btn.connect_clicked(move |_| {
                let (Some(app), Some(bar)) = (w.upgrade(), b.upgrade()) else { return };
                let package = bar.package.text().trim().to_owned();
                if package.is_empty() {
                    return app.toast("Type the app's package name, or pick it from the list");
                }
                let action = make(package);
                app.app_action(action);
            });
        }
        let w = weak.clone();
        shot.connect_clicked(move |_| {
            if let Some(app) = w.upgrade() {
                app.screenshot_all();
            }
        });
        let w = weak.clone();
        let b = Rc::downgrade(&bar);
        bar.record.connect_toggled(move |t| {
            let (Some(app), Some(bar)) = (w.upgrade(), b.upgrade()) else { return };
            if !bar.syncing.get() {
                app.send(UiCommand::Record { slots: app.online_slots(), on: t.is_active() });
            }
        });
        let w = weak;
        bar.graphs.connect_toggled(move |t| {
            if let Some(app) = w.upgrade() {
                app.set_graphs(t.is_active());
            }
        });
        bar
    }

    fn choose_apk(self: &Rc<Self>, app: &Rc<App>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Android apps (APK)"));
        filter.add_suffix("apk");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Choose an APK").modal(true).filters(&filters).default_filter(&filter).build();
        let b = Rc::downgrade(self);
        dialog.open(Some(app.window()), gio::Cancellable::NONE, move |r| {
            let (Ok(file), Some(bar)) = (r, b.upgrade()) else { return };
            if let Some(path) = file.path() {
                bar.set_apk(path);
            }
        });
    }

    /// The APK to install; its package fills the package box.
    pub fn set_apk(&self, path: PathBuf) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match ac_transfer::apk::info(&path) {
            Ok(info) => {
                let version = info.version_name.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
                self.apk_label.set_label(&format!("{name}{version}"));
                self.apk_label.set_tooltip_text(Some(&format!("{}\n{}", info.package, path.display())));
                self.package.set_text(&info.package);
            }
            Err(e) => {
                tracing::warn!("reading {}: {e}", path.display());
                self.apk_label.set_label(&name);
                self.apk_label.set_tooltip_text(Some(&path.display().to_string()));
            }
        }
        self.install.set_sensitive(true);
        *self.apk.borrow_mut() = Some(path);
    }

    pub fn add_packages(&self, packages: &[String]) {
        self.known.borrow_mut().extend(packages.iter().cloned());
        self.render_packages();
    }

    fn render_packages(&self) {
        while let Some(c) = self.packages_list.first_child() {
            self.packages_list.remove(&c);
        }
        let known = self.known.borrow();
        if known.is_empty() {
            let l = gtk::Label::builder().label("Asking the phones…").margin_top(8).margin_bottom(8).build();
            l.add_css_class("dim-label");
            self.packages_list.append(&l);
            return;
        }
        for p in known.iter() {
            let b = gtk::Button::builder().label(p).build();
            b.add_css_class("flat");
            b.child().and_downcast::<gtk::Label>().inspect(|l| l.set_xalign(0.0));
            let (entry, p) = (self.package.clone(), p.clone());
            b.connect_clicked(move |b| {
                entry.set_text(&p);
                if let Some(pop) = b.ancestor(gtk::Popover::static_type()).and_downcast::<gtk::Popover>() {
                    pop.popdown();
                }
            });
            self.packages_list.append(&b);
        }
    }

    pub fn set_run(self: &Rc<Self>, v: RunView) {
        let was = self.run.borrow().active;
        *self.run.borrow_mut() = v.clone();
        self.run_name.set_sensitive(!v.active);
        self.run_button.set_label(if v.active { "Stop run" } else { "Start run" });
        let (on, off) = if v.active { ("destructive-action", "suggested-action") } else { ("suggested-action", "destructive-action") };
        self.run_button.remove_css_class(off);
        self.run_button.add_css_class(on);
        self.open_run.set_visible(v.active);
        if v.active && !was {
            self.started.set(Some(Instant::now()));
            let b: Weak<Self> = Rc::downgrade(self);
            glib::timeout_add_local(Duration::from_secs(1), move || match b.upgrade() {
                Some(bar) if bar.run.borrow().active => {
                    bar.render_run();
                    glib::ControlFlow::Continue
                }
                _ => glib::ControlFlow::Break,
            });
        }
        if !v.active {
            self.started.set(None);
        }
        self.render_run();
    }

    fn render_run(&self) {
        let v = self.run.borrow();
        if !v.active {
            self.run_status.set_label("");
            return;
        }
        let secs = self.started.get().map_or(0, |t| t.elapsed().as_secs());
        let time = format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
        let crashes = match v.crashes {
            0 => String::new(),
            1 => " · 1 crash".into(),
            n => format!(" · {n} crashes"),
        };
        self.run_status.set_label(&format!("● {time}{crashes}"));
        self.run_status.set_tooltip_text(Some(&v.dir.display().to_string()));
        if v.crashes > 0 {
            self.run_status.add_css_class("error");
        } else {
            self.run_status.remove_css_class("error");
        }
    }

    pub fn run_active(&self) -> bool {
        self.run.borrow().active
    }

    /// The record toggle shows whether any phone is recording.
    pub fn set_recording(&self, any: bool) {
        self.syncing.set(true);
        self.record.set_active(any);
        self.syncing.set(false);
    }
}
