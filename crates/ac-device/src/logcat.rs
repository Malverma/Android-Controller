//! Crash and ANR detection from `logcat -v threadtime`, and the task that
//! streams a phone's log (into a test run's `logcat.txt` while one runs).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ac_adb::AdbClient;
use ac_core::SlotId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::api::{CrashKind, CrashView};

/// Lines of stack trace kept with a crash.
const MAX_DETAIL_LINES: usize = 40;

/// One `threadtime` line: `10-24 21:45:01.123  1234  1250 E Tag     : message`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line<'a> {
    pub time: &'a str,
    pub pid: u32,
    pub level: char,
    pub tag: &'a str,
    pub msg: &'a str,
}

pub fn parse_line(line: &str) -> Option<Line<'_>> {
    let line = line.trim_end_matches(['\r', '\n']);
    // `MM-DD hh:mm:ss.mmm`
    let time = line.get(..18)?;
    let b = time.as_bytes();
    if b[2] != b'-' || b[5] != b' ' || b[8] != b':' || b[14] != b'.' {
        return None;
    }
    let (pid, rest) = word(&line[18..])?;
    let (_tid, rest) = word(rest)?;
    let (level, rest) = word(rest)?;
    let (tag, msg) = rest.split_once(": ").or_else(|| rest.split_once(':'))?;
    Some(Line { time, pid: pid.parse().ok()?, level: level.chars().next()?, tag: tag.trim(), msg })
}

/// The next whitespace-separated word and what follows it.
fn word(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let end = s.find(char::is_whitespace)?;
    Some((&s[..end], &s[end..]))
}

/// Collects crash reports as their lines arrive.
#[derive(Debug, Default)]
pub struct CrashParser {
    pending: Option<Pending>,
}

#[derive(Debug)]
struct Pending {
    kind: CrashKind,
    tag: String,
    pid: u32,
    time: String,
    package: String,
    summary: String,
    detail: Vec<String>,
    /// Native: the crash dump's `signal` line has been seen.
    signal_seen: bool,
}

impl CrashParser {
    /// Feeds one line; returns a crash once its report is complete.
    pub fn push(&mut self, raw: &str) -> Option<CrashView> {
        let line = parse_line(raw)?;
        let mut done = None;
        if let Some(p) = &mut self.pending {
            let same = line.tag == p.tag && (p.kind == CrashKind::Native || line.pid == p.pid);
            if same && p.detail.len() < MAX_DETAIL_LINES {
                p.add(&line);
                if p.complete() {
                    done = self.finish();
                }
                return done;
            }
            done = self.finish();
        }
        self.pending = Pending::start(&line);
        done.or_else(|| self.pending.as_ref().is_some_and(Pending::complete).then(|| self.finish()).flatten())
    }

    /// The report in progress, when the log goes quiet.
    pub fn flush(&mut self) -> Option<CrashView> {
        self.finish()
    }

    fn finish(&mut self) -> Option<CrashView> {
        let p = self.pending.take()?;
        // scrcpy-server (or another shell tool) dying is not the app's crash.
        if p.package.is_empty() || p.package.starts_with("app_process") || p.detail.iter().any(|d| d.contains("com.genymobile.scrcpy")) {
            return None;
        }
        Some(CrashView {
            kind: p.kind,
            package: p.package,
            summary: if p.summary.is_empty() { p.kind.label().to_owned() } else { p.summary },
            time: p.time,
            detail: p.detail.join("\n"),
        })
    }
}

impl Pending {
    fn start(l: &Line) -> Option<Self> {
        let kind = match (l.tag, l.level) {
            ("AndroidRuntime", 'E') if l.msg.starts_with("FATAL EXCEPTION") => CrashKind::Crash,
            ("ActivityManager", 'E') if l.msg.starts_with("ANR in ") => CrashKind::Anr,
            ("DEBUG", 'F' | 'E') if l.msg.contains("*** *** ***") => CrashKind::Native,
            _ => return None,
        };
        let package = match kind {
            // `ANR in com.foo (com.foo/.MainActivity)`
            CrashKind::Anr => l.msg["ANR in ".len()..].split_whitespace().next().unwrap_or_default().to_owned(),
            _ => String::new(),
        };
        Some(Self {
            kind,
            tag: l.tag.to_owned(),
            pid: l.pid,
            time: l.time.to_owned(),
            package,
            summary: String::new(),
            detail: vec![l.msg.trim_end().to_owned()],
            signal_seen: false,
        })
    }

    fn add(&mut self, l: &Line) {
        let m = l.msg.trim_end();
        self.detail.push(m.to_owned());
        let t = m.trim();
        match self.kind {
            CrashKind::Crash => {
                if let Some(rest) = t.strip_prefix("Process: ") {
                    self.package = rest.split(',').next().unwrap_or_default().trim().to_owned();
                } else if self.summary.is_empty() && !self.package.is_empty() && !t.starts_with("at ") && !t.starts_with("PID:") {
                    self.summary = t.to_owned();
                }
            }
            CrashKind::Anr => {
                if let Some(r) = t.strip_prefix("Reason: ") {
                    self.summary = format!("Not responding: {r}");
                }
            }
            CrashKind::Native => {
                // `pid: 1234, tid: 1250, name: RenderThread  >>> com.foo <<<`
                if let Some((_, rest)) = t.split_once(">>> ")
                    && let Some((name, _)) = rest.split_once(" <<<")
                {
                    self.package = name.trim().to_owned();
                } else if t.starts_with("signal ") {
                    // `signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0`
                    self.signal_seen = true;
                    let sig = t.split(['(', ')']).nth(1).unwrap_or("signal");
                    self.summary = format!("Native crash ({sig})");
                } else if let Some(msg) = t.strip_prefix("Abort message: ") {
                    self.summary = format!("{} — {}", self.summary, msg.trim_matches('\''));
                }
            }
        }
    }

    /// Enough is known; the rest of a long trace is not worth waiting for.
    fn complete(&self) -> bool {
        match self.kind {
            CrashKind::Native => self.signal_seen && self.detail.len() >= 12,
            CrashKind::Anr => !self.summary.is_empty() && self.detail.len() >= 4,
            CrashKind::Crash => false,
        }
    }
}

/// What a slot's logcat task reports.
pub enum LogEvent {
    Crash(SlotId, CrashView),
    /// The stream ended (phone gone, logcat killed).
    Ended(SlotId),
}

/// `logcat` arguments: everything for a test run's file, else just the tags
/// that announce crashes. Starts at the newest line, not the old buffer.
pub fn logcat_cmd(sdk: u32, full: bool) -> String {
    // The `crash` buffer appeared in Android 7; asking for it before fails.
    let buffers = if sdk >= 24 { "-b main -b system -b crash" } else { "-b main -b system" };
    let filter = if full { "" } else { " AndroidRuntime:E ActivityManager:E DEBUG:F *:S" };
    format!("logcat -v threadtime -T 1 {buffers}{filter}")
}

/// Streams a phone's log until it ends: lines go to `file` (a test run's
/// `logcat.txt`) and through the crash parser.
pub async fn run(
    adb: AdbClient,
    serial: String,
    shell_v2: bool,
    sdk: u32,
    slot: SlotId,
    file: Option<PathBuf>,
    tx: tokio::sync::mpsc::UnboundedSender<LogEvent>,
) {
    let cmd = logcat_cmd(sdk, file.is_some());
    let service = if shell_v2 { format!("shell,v2,raw:{cmd}") } else { format!("shell:{cmd}") };
    let Ok(mut s) = adb.open_service(&serial, &service).await else {
        let _ = tx.send(LogEvent::Ended(slot));
        return;
    };
    let mut out = match &file {
        Some(p) => {
            if let Some(dir) = p.parent() {
                let _ = tokio::fs::create_dir_all(dir).await;
            }
            tokio::fs::OpenOptions::new().create(true).append(true).open(p).await.ok().map(tokio::io::BufWriter::new)
        }
        None => None,
    };
    let mut parser = CrashParser::default();
    let mut pending = Vec::new();
    // `-T 1` repeats the newest old line: never report from it.
    let mut first = true;
    let mut last_flush = Instant::now();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(1), async {
            if shell_v2 {
                match ac_adb::shell::read_packet(&mut s).await {
                    Ok(Some((1 | 2, data))) => Some(data),
                    Ok(Some((_, _))) => Some(Vec::new()),
                    _ => None,
                }
            } else {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => None,
                    Ok(n) => Some(buf[..n].to_vec()),
                }
            }
        })
        .await;
        match read {
            Ok(Some(data)) => pending.extend_from_slice(&data),
            Ok(None) => break,
            // Quiet log: a crash report waiting for its next line is done.
            Err(_) => {
                if let Some(c) = parser.flush() {
                    let _ = tx.send(LogEvent::Crash(slot, c));
                }
            }
        }
        while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = pending.drain(..=nl).collect();
            let text = String::from_utf8_lossy(&line);
            if let Some(o) = &mut out
                && o.write_all(text.trim_end_matches('\r').as_bytes()).await.is_err()
            {
                out = None;
            }
            if std::mem::take(&mut first) {
                continue;
            }
            if let Some(c) = parser.push(&text) {
                let _ = tx.send(LogEvent::Crash(slot, c));
            }
        }
        if last_flush.elapsed() >= Duration::from_secs(1) {
            last_flush = Instant::now();
            if let Some(o) = &mut out {
                let _ = o.flush().await;
            }
        }
    }
    if let Some(c) = parser.flush() {
        let _ = tx.send(LogEvent::Crash(slot, c));
    }
    if let Some(o) = &mut out {
        let _ = o.flush().await;
    }
    let _ = tx.send(LogEvent::Ended(slot));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(lines: &str) -> Vec<CrashView> {
        let mut p = CrashParser::default();
        let mut out: Vec<CrashView> = lines.lines().filter_map(|l| p.push(l)).collect();
        out.extend(p.flush());
        out
    }

    #[test]
    fn parses_threadtime_lines() {
        let l = parse_line("10-24 21:45:01.123  1234  1250 E AndroidRuntime: FATAL EXCEPTION: main").unwrap();
        assert_eq!((l.time, l.pid, l.level, l.tag, l.msg), ("10-24 21:45:01.123", 1234, 'E', "AndroidRuntime", "FATAL EXCEPTION: main"));
        let l = parse_line("10-24 18:04:16.182 16813 16813 F DEBUG   : *** *** ***").unwrap();
        assert_eq!((l.tag, l.msg), ("DEBUG", "*** *** ***"));
        assert!(parse_line("--------- beginning of crash").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn java_crash() {
        let log = "\
10-24 21:45:01.120  1234  1234 D Game    : loading level 3
10-24 21:45:01.123  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main
10-24 21:45:01.123  1234  1234 E AndroidRuntime: Process: com.example.game, PID: 1234
10-24 21:45:01.123  1234  1234 E AndroidRuntime: java.lang.NullPointerException: Attempt to invoke virtual method on a null object
10-24 21:45:01.123  1234  1234 E AndroidRuntime: \tat com.example.game.Level.load(Level.java:42)
10-24 21:45:01.130   900   950 I ActivityManager: Showing crash dialog";
        let c = feed(log);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].kind, CrashKind::Crash);
        assert_eq!(c[0].package, "com.example.game");
        assert_eq!(c[0].summary, "java.lang.NullPointerException: Attempt to invoke virtual method on a null object");
        assert_eq!(c[0].time, "10-24 21:45:01.123");
        assert!(c[0].detail.contains("Level.java:42"));
    }

    #[test]
    fn anr() {
        let log = "\
10-24 21:50:00.001   900   990 E ActivityManager: ANR in com.example.game (com.example.game/.MainActivity)
10-24 21:50:00.001   900   990 E ActivityManager: PID: 4321
10-24 21:50:00.001   900   990 E ActivityManager: Reason: Input dispatching timed out
10-24 21:50:00.001   900   990 E ActivityManager: Load: 3.2 / 2.1 / 1.0";
        let c = feed(log);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].kind, c[0].package.as_str()), (CrashKind::Anr, "com.example.game"));
        assert_eq!(c[0].summary, "Not responding: Input dispatching timed out");
    }

    #[test]
    fn native_crash_of_an_app() {
        let mut log = String::from(
            "\
10-24 18:04:16.182 16813 16813 F DEBUG   : *** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***
10-24 18:04:16.182 16813 16813 F DEBUG   : Build fingerprint: 'motorola/maui_retail/maui:13/T3SGS33/71b19:user/release-keys'
10-24 18:04:16.182 16813 16813 F DEBUG   : pid: 16796, tid: 16810, name: RenderThread  >>> com.example.game <<<
10-24 18:04:16.182 16813 16813 F DEBUG   : uid: 10123
10-24 18:04:16.183 16813 16813 F DEBUG   : signal 11 (SIGSEGV), code 1 (SEGV_MAPERR), fault addr 0x0
",
        );
        for i in 0..10 {
            log.push_str(&format!("10-24 18:04:16.183 16813 16813 F DEBUG   :       #{i:02} pc 00039b52  /data/app/lib/libgame.so\n"));
        }
        let c = feed(&log);
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!((c[0].kind, c[0].package.as_str()), (CrashKind::Native, "com.example.game"));
        assert_eq!(c[0].summary, "Native crash (SIGSEGV)");
    }

    #[test]
    fn scrcpy_server_dying_is_not_an_app_crash() {
        // The real Moto log: the mirroring agent aborted while starting.
        let log = "\
10-24 18:04:16.182 16813 16813 F DEBUG   : *** *** *** *** *** *** *** *** *** *** *** *** *** *** *** ***
10-24 18:04:16.182 16813 16813 F DEBUG   : Cmdline: app_process / com.genymobile.scrcpy.Server 4.1 scid=58a2b95a
10-24 18:04:16.182 16813 16813 F DEBUG   : pid: 16796, tid: 16796, name: main  >>> app_process <<<
10-24 18:04:16.183 16813 16813 F DEBUG   : signal 6 (SIGABRT), code -1 (SI_QUEUE), fault addr --------";
        assert!(feed(log).is_empty());
    }

    #[test]
    fn two_crashes_back_to_back() {
        let log = "\
10-24 21:45:01.123  1234  1234 E AndroidRuntime: FATAL EXCEPTION: main
10-24 21:45:01.123  1234  1234 E AndroidRuntime: Process: com.a, PID: 1234
10-24 21:45:01.123  1234  1234 E AndroidRuntime: java.lang.IllegalStateException: boom
10-24 21:45:02.123  5678  5678 E AndroidRuntime: FATAL EXCEPTION: worker
10-24 21:45:02.123  5678  5678 E AndroidRuntime: Process: com.b, PID: 5678
10-24 21:45:02.123  5678  5678 E AndroidRuntime: java.lang.OutOfMemoryError: Failed to allocate";
        let c = feed(log);
        assert_eq!(c.iter().map(|c| c.package.as_str()).collect::<Vec<_>>(), ["com.a", "com.b"]);
        assert_eq!(c[1].summary, "java.lang.OutOfMemoryError: Failed to allocate");
    }

    #[test]
    fn commands() {
        assert_eq!(logcat_cmd(33, false), "logcat -v threadtime -T 1 -b main -b system -b crash AndroidRuntime:E ActivityManager:E DEBUG:F *:S");
        assert_eq!(logcat_cmd(23, true), "logcat -v threadtime -T 1 -b main -b system");
    }
}
