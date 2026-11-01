//! The virtual monitor, through `hyprctl`. Nothing is written to the user's
//! Hyprland config: an output lives until it is removed or Hyprland exits.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// True inside a Hyprland session.
pub fn available() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|s| !s.is_empty())
}

fn hyprctl(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("hyprctl").args(args).output().context("running hyprctl")?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if !out.status.success() {
        bail!("hyprctl {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(text)
}

/// hyprctl reports most failures with exit status 0 and a message instead of "ok".
fn expect_ok(args: &[&str]) -> Result<()> {
    let reply = hyprctl(args)?;
    if reply != "ok" {
        bail!("hyprctl {}: {reply}", args.join(" "));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MonitorInfo {
    pub name: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub x: i32,
    #[serde(default)]
    pub y: i32,
    #[serde(default = "one")]
    pub scale: f64,
}

fn one() -> f64 {
    1.0
}

pub fn parse_monitors(json: &str) -> Result<Vec<MonitorInfo>> {
    serde_json::from_str(json).context("parsing hyprctl monitors")
}

pub fn monitors() -> Result<Vec<MonitorInfo>> {
    parse_monitors(&hyprctl(&["-j", "monitors"])?)
}

pub fn exists(name: &str) -> bool {
    monitors().is_ok_and(|m| m.iter().any(|m| m.name == name))
}

/// The value for `hyprctl keyword monitor`.
pub fn monitor_rule(name: &str, width: u32, height: u32, fps: u32, position: &str, scale: f64) -> String {
    format!("{name},{width}x{height}@{fps},{position},{scale}")
}

/// Creates the headless output if needed and gives it this mode.
pub fn create(name: &str, rule: &str) -> Result<()> {
    if !exists(name) {
        expect_ok(&["output", "create", "headless", name])?;
    }
    expect_ok(&["keyword", "monitor", rule])
}

pub fn remove(name: &str) -> Result<()> {
    if exists(name) {
        expect_ok(&["output", "remove", name])?;
    }
    Ok(())
}

/// Removes outputs left behind by a crash. Returns how many.
pub fn remove_leftovers() -> Result<usize> {
    let stale: Vec<String> =
        monitors()?.into_iter().map(|m| m.name).filter(|n| n.starts_with(crate::OUTPUT_PREFIX)).collect();
    for name in &stale {
        remove(name)?;
    }
    Ok(stale.len())
}

/// The mode for a phone: landscape unless asked otherwise, longest side capped,
/// both sides even as H.264 requires.
pub fn phone_mode(width: u32, height: u32, portrait: bool, max_side: u32) -> (u32, u32) {
    let (long, short) = (width.max(height), width.min(height));
    let scale = if long > max_side && long > 0 { f64::from(max_side) / f64::from(long) } else { 1.0 };
    let even = |v: f64| (v.round() as u32) & !1;
    let (long, short) = (even(f64::from(long) * scale), even(f64::from(short) * scale));
    if portrait { (short, long) } else { (long, short) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_monitors() {
        let m = parse_monitors(r#"[{"name":"eDP-1","width":1920,"height":1080,"x":0,"y":0,"scale":1.0,"extra":true},{"name":"TBM-2","width":1920,"height":864}]"#).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!((m[1].name.as_str(), m[1].scale), ("TBM-2", 1.0));
    }

    #[test]
    fn builds_the_rule() {
        assert_eq!(monitor_rule("TBM-1", 1920, 864, 60, "auto-right", 1.5), "TBM-1,1920x864@60,auto-right,1.5");
    }

    #[test]
    fn phone_modes() {
        // A 1080x2400 phone reports portrait; a monitor defaults to landscape.
        assert_eq!(phone_mode(1080, 2400, false, 1920), (1920, 864));
        assert_eq!(phone_mode(1080, 2400, true, 1920), (864, 1920));
        // Already within the cap: unchanged.
        assert_eq!(phone_mode(720, 1280, false, 1920), (1280, 720));
        // Odd sizes become even.
        assert_eq!(phone_mode(721, 1281, false, 1920), (1280, 720));
    }
}
