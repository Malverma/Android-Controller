//! Phone performance while testing: frame rate of the app on screen, CPU,
//! memory and temperature, sampled over ADB every couple of seconds.

use std::collections::HashMap;

use crate::api::PerfSample;

/// Foreground app and its SurfaceFlinger layers (refreshed now and then:
/// listing layers costs more than reading one).
pub const LAYERS_CMD: &str = "echo @top; dumpsys activity activities 2>/dev/null | grep -m 1 -E 'mResumedActivity|topResumedActivity'; \
echo @layers; dumpsys SurfaceFlinger --list 2>/dev/null";

/// One sample. `layers` are read with `--latency` for the frame rate, `app`
/// is the package whose memory is wanted.
pub fn sample_cmd(layers: &[String], app: Option<&str>) -> String {
    let mut cmd = String::from(
        "echo @stat; head -1 /proc/stat; \
echo @mem; grep -E '^(MemTotal|MemAvailable):' /proc/meminfo; \
echo @battery; dumpsys battery 2>/dev/null | grep -m 1 temperature; \
echo @thermal; for z in /sys/class/thermal/thermal_zone*; do echo \"$(cat $z/type 2>/dev/null) $(cat $z/temp 2>/dev/null)\"; done 2>/dev/null",
    );
    for (i, l) in layers.iter().enumerate() {
        cmd.push_str(&format!("; echo @layer{i}; dumpsys SurfaceFlinger --latency {}", quote(l)));
    }
    if let Some(app) = app {
        cmd.push_str(&format!("; echo @app; dumpsys meminfo {} 2>/dev/null | grep -m 1 -E 'TOTAL( PSS)?:? +[0-9]'", quote(app)));
    }
    cmd
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn sections(out: &str) -> HashMap<&str, Vec<&str>> {
    let mut map: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut cur = None;
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(name) = line.strip_prefix('@')
            && !name.is_empty()
            && !name.contains(' ')
        {
            cur = Some(name);
            map.entry(name).or_default();
        } else if let Some(c) = cur {
            map.get_mut(c).unwrap().push(line);
        }
    }
    map
}

/// The app on screen and the layers that may carry its frames: a game's
/// `SurfaceView` first, then the window itself.
pub fn parse_layers(out: &str) -> (Option<String>, Vec<String>) {
    let s = sections(out);
    // `topResumedActivity=ActivityRecord{9cd16c u0 bitpit.launcher/.ui.HomeActivity t13}`
    let comp = s.get("top").and_then(|l| l.first()).and_then(|l| l.split_whitespace().find(|w| w.contains('/')));
    let Some((pkg, class)) = comp.and_then(|c| c.split_once('/')) else { return (None, Vec::new()) };
    let pkg = pkg.to_owned();
    // `.Settings` is short for `com.android.settings.Settings`. Layers of
    // the resumed activity come first: the app's older activities keep
    // theirs while they sit in the back stack.
    let activity = format!("{pkg}/{}", if class.starts_with('.') { format!("{pkg}{class}") } else { class.to_owned() });
    let mut surface = Vec::new();
    let mut window = Vec::new();
    for l in s.get("layers").into_iter().flatten().map(|l| l.trim()) {
        if !l.contains(&pkg)
            || l.starts_with("ActivityRecord{")
            || l.starts_with("Surface(name=")
            || l.contains("ActivityRecordInputSink")
            || l.contains("animation-leash")
            || l.contains("Splash Screen")
        {
            continue;
        }
        if l.contains("SurfaceView") && !l.contains("Background") {
            surface.push(l.to_owned());
        } else if l.split_whitespace().count() <= 2 {
            window.push(l.to_owned());
        }
    }
    surface.extend(window);
    let mine = |l: &str| l.match_indices(&activity).any(|(i, m)| matches!(l[i + m.len()..].chars().next(), None | Some('#' | ' ' | ']')));
    surface.sort_by_key(|l| !mine(l));
    surface.truncate(3);
    (Some(pkg), surface)
}

/// What a sample needs from the one before it.
#[derive(Debug, Default, Clone)]
pub struct PerfState {
    /// `/proc/stat` busy and total jiffies.
    cpu: Option<(u64, u64)>,
    /// Newest frame time seen per layer.
    newest: HashMap<usize, i64>,
}

pub fn parse_sample(out: &str, state: &mut PerfState, app: Option<&str>) -> PerfSample {
    let s = sections(out);
    let mut p = PerfSample { app: app.map(str::to_owned), ..Default::default() };
    // `cpu  user nice system idle iowait irq softirq steal …`
    if let Some(line) = s.get("stat").and_then(|l| l.first()) {
        let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        if v.len() >= 4 {
            let total: u64 = v.iter().sum();
            let idle = v[3] + v.get(4).copied().unwrap_or(0);
            let busy = total - idle;
            if let Some((b0, t0)) = state.cpu
                && total > t0
            {
                p.cpu_pct = Some(((busy.saturating_sub(b0)) as f32 / (total - t0) as f32 * 100.0).clamp(0.0, 100.0));
            }
            state.cpu = Some((busy, total));
        }
    }
    let kb = |l: &str| l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok());
    let mem = s.get("mem").map(|l| l.as_slice()).unwrap_or_default();
    let total = mem.iter().find(|l| l.starts_with("MemTotal")).and_then(|l| kb(l));
    let avail = mem.iter().find(|l| l.starts_with("MemAvailable")).and_then(|l| kb(l));
    if let (Some(t), Some(a)) = (total, avail) {
        p.mem_total_mb = Some((t / 1024) as u32);
        p.mem_used_mb = Some((t.saturating_sub(a) / 1024) as u32);
    }
    // `  temperature: 312` in tenths of a degree.
    p.battery_temp_c = s
        .get("battery")
        .and_then(|l| l.first())
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse::<f32>().ok())
        .map(|t| t / 10.0);
    // Hottest CPU zone; values are millidegrees (some kernels: degrees).
    p.cpu_temp_c = s
        .get("thermal")
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let (ty, v) = l.trim().rsplit_once(' ')?;
            let ty = ty.to_ascii_lowercase();
            (ty.contains("cpu") || ty.contains("tsens") || ty.contains("soc")).then_some(())?;
            let v: f32 = v.parse().ok()?;
            let c = if v > 1000.0 { v / 1000.0 } else { v };
            (5.0..150.0).contains(&c).then_some(c)
        })
        .reduce(f32::max);
    // `--latency`: refresh period, then up to 127 rows of
    // `desired actual(present) frame-ready` in ns; INT64_MAX = pending.
    let mut best: Option<f32> = None;
    for i in 0..8 {
        let Some(rows) = s.get(format!("layer{i}").as_str()) else { continue };
        let times: Vec<i64> = rows
            .iter()
            .skip(1)
            .filter_map(|r| r.split_whitespace().nth(1)?.parse::<i64>().ok())
            .filter(|&t| t > 0 && t != i64::MAX)
            .collect();
        let Some(&newest) = times.iter().max() else { continue };
        let before = state.newest.insert(i, newest);
        // No new frame since the last sample: the app is not drawing.
        let fps = if before == Some(newest) {
            0.0
        } else {
            times.iter().filter(|&&t| t > newest - 1_000_000_000).count() as f32
        };
        best = Some(best.map_or(fps, |b: f32| b.max(fps)));
    }
    p.fps = best;
    // `TOTAL PSS:   123456 …` (Android 10+) or `TOTAL   123456 …` in KiB.
    p.app_mem_mb = s
        .get("app")
        .and_then(|l| l.first())
        .and_then(|l| l.split(|c: char| !c.is_ascii_digit()).find(|w| !w.is_empty()))
        .and_then(|v| v.parse::<u64>().ok())
        .map(|kib| (kib / 1024) as u32);
    p
}

/// `stats.csv` in a test run's phone folder.
pub const CSV_HEADER: &str = "time,fps,cpu_pct,mem_used_mb,mem_total_mb,app_mem_mb,cpu_temp_c,battery_temp_c\n";

pub fn csv_row(time: &str, p: &PerfSample) -> String {
    fn f<T: std::fmt::Display>(v: Option<T>) -> String {
        v.map(|v| v.to_string()).unwrap_or_default()
    }
    let r1 = |v: Option<f32>| v.map(|v| (v * 10.0).round() / 10.0);
    format!(
        "{time},{},{},{},{},{},{},{}\n",
        f(r1(p.fps)),
        f(r1(p.cpu_pct)),
        f(p.mem_used_mb),
        f(p.mem_total_mb),
        f(p.app_mem_mb),
        f(r1(p.cpu_temp_c)),
        f(r1(p.battery_temp_c))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumed_activity_layers_first() {
        // Android 11 (Schok): an older Settings activity is still listed.
        let out = "@top\n    mResumedActivity: ActivityRecord{107932d u0 com.android.settings/.Settings t168}\n@layers\n\
de918a4 com.android.settings/com.android.settings.Settings$WifiSettings2Activity#0\n\
com.android.settings/com.android.settings.Settings$WifiSettings2Activity#0\n\
91b6a46 com.android.settings/com.android.settings.Settings#0\n\
com.android.settings/com.android.settings.Settings#0\n";
        let (top, layers) = parse_layers(out);
        assert_eq!(top.as_deref(), Some("com.android.settings"));
        assert_eq!(layers[0], "91b6a46 com.android.settings/com.android.settings.Settings#0");
        assert_eq!(layers[1], "com.android.settings/com.android.settings.Settings#0");
    }

    #[test]
    fn foreground_layers() {
        // Trimmed from the Moto (Android 13).
        let out = "@top
      topResumedActivity=ActivityRecord{9cd16c u0 com.example.game/.MainActivity t13}
@layers
Display 0 name=\"Built-in Screen\"#3
ActivityRecord{9cd16c u0 com.example.game/.MainActivity}#926
1c4335 ActivityRecordInputSink com.example.game/.MainActivity#937
d3d3bd8 com.example.game/com.example.game.MainActivity#2690
com.example.game/com.example.game.MainActivity#3846
SurfaceView[com.example.game/com.example.game.MainActivity]#3850
Background for SurfaceView[com.example.game/com.example.game.MainActivity]#3851
bitpit.launcher/bitpit.launcher.ui.HomeActivity#3000";
        let (pkg, layers) = parse_layers(out);
        assert_eq!(pkg.as_deref(), Some("com.example.game"));
        assert_eq!(
            layers,
            [
                "SurfaceView[com.example.game/com.example.game.MainActivity]#3850",
                "d3d3bd8 com.example.game/com.example.game.MainActivity#2690",
                "com.example.game/com.example.game.MainActivity#3846",
            ]
        );
        assert_eq!(parse_layers("@top\n@layers\nfoo#1"), (None, vec![]));
    }

    fn latency(start_ns: i64, frames: usize, step_ns: i64) -> String {
        let mut s = String::from("16666667\n");
        for i in 0..frames {
            let t = start_ns + i as i64 * step_ns;
            s.push_str(&format!("{}\t{t}\t{}\n", t - 1000, t - 500));
        }
        s
    }

    #[test]
    fn sample_math() {
        let mut st = PerfState::default();
        let first = format!(
            "@stat\ncpu  100 0 100 800 0 0 0 0 0 0\n@mem\nMemTotal:        2856540 kB\nMemAvailable:    1108608 kB\n\
@battery\n  temperature: 312\n@thermal\nxo-therm-adc 31000\napc1-cpu2-usr 33000\napc1-cpu1-step 35000\nbattery 30000\n \n\
@layer0\n{}@app\n        TOTAL PSS:   123456            TOTAL RSS:   200000\n",
            latency(1_000_000_000, 60, 16_666_667)
        );
        let p = parse_sample(&first, &mut st, Some("com.example.game"));
        assert_eq!(p.cpu_pct, None, "needs two samples");
        assert_eq!((p.mem_used_mb, p.mem_total_mb), (Some(1706), Some(2789)));
        assert_eq!(p.battery_temp_c, Some(31.2));
        assert_eq!(p.cpu_temp_c, Some(35.0));
        assert_eq!(p.fps, Some(60.0));
        assert_eq!(p.app_mem_mb, Some(120));
        // 1 s later: 50 % busy, 30 fps, and the same layer frames again → 0 fps.
        let second = format!("@stat\ncpu  150 0 150 900 0 0 0 0 0 0\n@layer0\n{}", latency(3_000_000_000, 30, 33_333_333));
        let p = parse_sample(&second, &mut st, None);
        assert_eq!(p.cpu_pct, Some(50.0));
        assert_eq!(p.fps, Some(30.0));
        assert_eq!(p.mem_used_mb, None);
        let p = parse_sample(&second, &mut st, None);
        assert_eq!(p.fps, Some(0.0), "nothing drawn since");
    }

    #[test]
    fn old_meminfo_format_and_csv() {
        let mut st = PerfState::default();
        let p = parse_sample("@app\n    TOTAL    81234    60000\n", &mut st, Some("a.b"));
        assert_eq!(p.app_mem_mb, Some(79));
        let p = PerfSample { fps: Some(59.94), cpu_pct: Some(12.345), mem_used_mb: Some(1700), ..Default::default() };
        assert_eq!(csv_row("21:50:01", &p), "21:50:01,59.9,12.3,1700,,,,\n");
    }
}
