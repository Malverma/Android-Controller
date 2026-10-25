//! App testing on every phone at once (Milestone 5): test runs and where
//! they are saved, app actions (launch, stop, clear data, uninstall) and
//! screenshots.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

use crate::api::{AppAction, CrashView};

/// Where test runs, screenshots and recordings go: `testing.output_dir`,
/// or `~/Documents/Android Controller` (`~/Android Controller` without a
/// Documents folder).
pub fn output_root(configured: &str) -> PathBuf {
    let configured = configured.trim();
    if !configured.is_empty() {
        if let Some(rest) = configured.strip_prefix("~/")
            && let Some(home) = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
        {
            return home.join(rest);
        }
        return PathBuf::from(configured);
    }
    let dirs = directories::UserDirs::new();
    match dirs.as_ref().and_then(|d| d.document_dir()) {
        Some(docs) => docs.join("Android Controller"),
        None => dirs.map_or_else(|| PathBuf::from("Android Controller"), |d| d.home_dir().join("Android Controller")),
    }
}

/// A file or folder name from a phone's or run's name: no `/`, no leading dot.
pub fn safe_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c == '/' || c == '\\' || c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    if cleaned.is_empty() { "phone".to_owned() } else { cleaned.chars().take(80).collect() }
}

/// A running test run.
pub struct Run {
    pub name: String,
    pub dir: PathBuf,
    pub started: DateTime<Local>,
    /// Per slot: the phone's folder and what happened to it.
    pub phones: Vec<Option<RunPhone>>,
}

pub struct RunPhone {
    pub title: String,
    pub dir: PathBuf,
    /// No ADB: Android does not let an app read the system log.
    pub logs: bool,
    pub crashes: Vec<CrashView>,
}

impl Run {
    pub fn new(root: &Path, name: &str, slots: usize, now: DateTime<Local>) -> std::io::Result<Self> {
        let stamp = now.format("%Y-%m-%d %H-%M-%S").to_string();
        let name = name.trim();
        let folder = if name.is_empty() { stamp } else { format!("{stamp} {}", safe_name(name)) };
        let dir = root.join("Runs").join(folder);
        std::fs::create_dir_all(&dir)?;
        Ok(Self { name: name.to_owned(), dir, started: now, phones: (0..slots).map(|_| None).collect() })
    }

    /// The phone's folder (made on first use), `"<slot> <name>"`.
    pub fn phone(&mut self, i: usize, title: &str, logs: bool) -> &mut RunPhone {
        let dir = self.dir.join(format!("{} {}", i + 1, safe_name(title)));
        let p = self.phones[i].get_or_insert_with(|| RunPhone { title: title.to_owned(), dir: dir.clone(), logs, crashes: Vec::new() });
        let _ = std::fs::create_dir_all(&p.dir);
        p
    }

    pub fn crash_count(&self) -> usize {
        self.phones.iter().flatten().map(|p| p.crashes.len()).sum()
    }

    /// `summary.txt`: what ran where and what went wrong.
    pub fn summary(&self, ended: DateTime<Local>) -> String {
        let mut s = String::new();
        let took = ended.signed_duration_since(self.started);
        s.push_str(&format!("Test run: {}\n", if self.name.is_empty() { "(unnamed)" } else { &self.name }));
        s.push_str(&format!("Started:  {}\nEnded:    {}\n", self.started.format("%Y-%m-%d %H:%M:%S"), ended.format("%Y-%m-%d %H:%M:%S")));
        s.push_str(&format!("Duration: {}:{:02}:{:02}\n", took.num_hours(), took.num_minutes() % 60, took.num_seconds() % 60));
        let n = self.crash_count();
        s.push_str(&format!("Crashes and ANRs: {n}\n"));
        for p in self.phones.iter().flatten() {
            s.push_str(&format!("\n== {} ==\n", p.title));
            if !p.logs {
                s.push_str("No system log: this phone is connected without ADB.\n");
            }
            if p.crashes.is_empty() {
                s.push_str("No crashes.\n");
            }
            for c in &p.crashes {
                s.push_str(&format!("{}  {}  {}: {}\n", c.time, c.kind.label(), c.package, c.summary));
            }
        }
        s
    }
}

/// `crashes.txt` entry.
pub fn crash_entry(c: &CrashView) -> String {
    format!("==== {} {} in {} ====\n{}\n{}\n\n", c.time, c.kind.label(), c.package, c.summary, c.detail)
}

/// A screenshot or recording file name: `<what> 21-50-03.png`.
pub fn capture_name(what: &str, now: DateTime<Local>, ext: &str) -> String {
    format!("{what} {}.{ext}", now.format("%H-%M-%S"))
}

/// Shell command for an app action over ADB.
pub fn app_cmd(action: &AppAction) -> String {
    let q = |p: &str| format!("'{}'", p.replace('\'', r"'\''"));
    match action {
        // `monkey` resolves the launcher activity on every Android version.
        AppAction::Launch(p) => format!("monkey -p {} -c android.intent.category.LAUNCHER 1 2>&1", q(p)),
        AppAction::Stop(p) => format!("am force-stop {} 2>&1 && echo @ok", q(p)),
        AppAction::ClearData(p) => format!("pm clear {} 2>&1", q(p)),
        AppAction::Uninstall(p) => format!("pm uninstall {} 2>&1", q(p)),
    }
}

/// Did it work? `Err` carries the reason for the user.
pub fn app_result(action: &AppAction, out: &str) -> Result<String, String> {
    let has = |s: &str| out.lines().any(|l| l.trim() == s);
    let failure = || {
        out.split_once("Failure [")
            .and_then(|(_, r)| r.split_once(']'))
            .map(|(r, _)| r.trim().to_owned())
            .unwrap_or_else(|| out.trim().lines().last().unwrap_or("no answer from the phone").trim().to_owned())
    };
    match action {
        AppAction::Launch(_) if out.contains("Events injected: 1") => Ok("Launched".into()),
        AppAction::Launch(_) if out.contains("No activities found") => Err("The app is not installed (or has no launcher icon)".into()),
        AppAction::Launch(_) => Err(format!("Launch failed: {}", failure())),
        AppAction::Stop(_) if has("@ok") => Ok("Stopped".into()),
        AppAction::Stop(_) => Err(format!("Stop failed: {}", failure())),
        AppAction::ClearData(_) if has("Success") => Ok("Data cleared".into()),
        AppAction::ClearData(_) => Err(format!("Clear data failed: {}", failure())),
        AppAction::Uninstall(_) if has("Success") => Ok("Uninstalled".into()),
        AppAction::Uninstall(_) if out.contains("not installed") || out.contains("DELETE_FAILED_INTERNAL_ERROR") => {
            Err("The app is not installed".into())
        }
        AppAction::Uninstall(_) => Err(format!("Uninstall failed: {}", failure())),
    }
}

/// `pm list packages -3` → sorted package names.
pub fn parse_packages(out: &str) -> Vec<String> {
    let mut v: Vec<String> =
        out.lines().filter_map(|l| l.trim().strip_prefix("package:")).map(|p| p.trim().to_owned()).filter(|p| !p.is_empty()).collect();
    v.sort();
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::CrashKind;
    use chrono::TimeZone;

    #[test]
    fn names() {
        assert_eq!(safe_name("moto g/play: 2023"), "moto g_play_ 2023");
        assert_eq!(safe_name("  ..hidden "), "hidden");
        assert_eq!(safe_name("///"), "___");
        assert_eq!(safe_name(""), "phone");
        assert_eq!(output_root("/tmp/x"), PathBuf::from("/tmp/x"));
        assert!(output_root("").ends_with("Android Controller"));
    }

    #[test]
    fn run_folders_and_summary() {
        let root = std::env::temp_dir().join(format!("ac-run-test-{}", std::process::id()));
        let t0 = Local.with_ymd_and_hms(2026, 10, 24, 21, 50, 3).unwrap();
        let mut run = Run::new(&root, "Level 3 / boss", 4, t0).unwrap();
        assert_eq!(run.dir, root.join("Runs").join("2026-10-24 21-50-03 Level 3 _ boss"));
        let want = run.dir.join("2 Moto");
        let p = run.phone(1, "Moto", true);
        assert_eq!(p.dir, want);
        p.crashes.push(CrashView {
            kind: CrashKind::Crash,
            package: "com.example.game".into(),
            summary: "java.lang.NullPointerException".into(),
            time: "10-24 21:51:00.000".into(),
            detail: String::new(),
        });
        run.phone(0, "LG K20", false);
        let s = run.summary(t0 + chrono::Duration::seconds(3725));
        assert!(s.contains("Duration: 1:02:05"), "{s}");
        assert!(s.contains("Crashes and ANRs: 1"));
        assert!(s.contains("== LG K20 ==\nNo system log"));
        assert!(s.contains("10-24 21:51:00.000  Crash  com.example.game: java.lang.NullPointerException"));
        assert_eq!(capture_name("Screenshot", t0, "png"), "Screenshot 21-50-03.png");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn app_actions() {
        let launch = AppAction::Launch("com.a".into());
        assert_eq!(app_cmd(&launch), "monkey -p 'com.a' -c android.intent.category.LAUNCHER 1 2>&1");
        assert_eq!(app_result(&launch, "  bash arg: -p\n Events injected: 1\n## Network stats"), Ok("Launched".into()));
        assert!(app_result(&launch, "** No activities found to run, monkey aborted.").unwrap_err().contains("not installed"));
        let un = AppAction::Uninstall("com.a".into());
        assert_eq!(app_result(&un, "Success\n"), Ok("Uninstalled".into()));
        assert!(app_result(&un, "Failure [DELETE_FAILED_INTERNAL_ERROR]").unwrap_err().contains("not installed"));
        assert_eq!(app_result(&un, "Failure [DELETE_FAILED_DEVICE_POLICY_MANAGER]"), Err("Uninstall failed: DELETE_FAILED_DEVICE_POLICY_MANAGER".into()));
        assert_eq!(app_result(&AppAction::ClearData("a".into()), "Success"), Ok("Data cleared".into()));
        assert_eq!(app_result(&AppAction::Stop("a".into()), "@ok"), Ok("Stopped".into()));
        assert_eq!(parse_packages("package:com.b\r\npackage:com.a\npackage:com.b\n"), ["com.a", "com.b"]);
    }
}
