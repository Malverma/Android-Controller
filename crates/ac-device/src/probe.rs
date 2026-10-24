//! Device-side commands and their output parsers: identity, battery, Wi-Fi.

use ac_core::DeviceStats;

/// One shell round trip for identity. Sections are separated by marker lines
/// so a missing tool on one ROM does not shift the others.
pub const IDENTIFY_CMD: &str = "echo @serial; getprop ro.serialno; \
echo @model; getprop ro.product.model; \
echo @manufacturer; getprop ro.product.manufacturer; \
echo @sdk; getprop ro.build.version.sdk; \
echo @aid; settings get secure android_id 2>/dev/null; \
echo @mac; ip -o link show wlan0 2>/dev/null";

pub const STATS_CMD: &str = "echo @battery; dumpsys battery 2>/dev/null; \
echo @wifi; cmd wifi status 2>/dev/null | grep -m 3 -iE 'rssi|link speed' || \
dumpsys wifi 2>/dev/null | grep -m 1 'mWifiInfo'";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub id: String,
    pub model: String,
    pub manufacturer: String,
    pub sdk: u32,
    pub wifi_mac: Option<String>,
}

fn sections(out: &str) -> impl Iterator<Item = (&str, Vec<&str>)> {
    let mut parts = Vec::new();
    let mut current: Option<(&str, Vec<&str>)> = None;
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(name) = line.strip_prefix('@')
            && !name.contains(' ')
        {
            parts.extend(current.take());
            current = Some((name, Vec::new()));
        } else if let Some((_, lines)) = current.as_mut() {
            lines.push(line);
        }
    }
    parts.extend(current);
    parts.into_iter()
}

pub fn parse_identity(out: &str) -> Option<Identity> {
    let mut id = Identity::default();
    let mut aid = String::new();
    for (name, lines) in sections(out) {
        let first = lines.first().map(|l| l.trim()).unwrap_or("").to_owned();
        match name {
            "serial" => id.id = first,
            "model" => id.model = first,
            "manufacturer" => id.manufacturer = first,
            "sdk" => id.sdk = first.parse().unwrap_or(0),
            "aid" => aid = first,
            "mac" => id.wifi_mac = lines.iter().find_map(|l| mac_from_ip_link(l)),
            _ => {}
        }
    }
    if id.id.is_empty() || id.id.eq_ignore_ascii_case("unknown") {
        if aid.is_empty() || aid == "null" {
            return None;
        }
        id.id = format!("aid:{aid}");
    }
    Some(id)
}

fn mac_from_ip_link(line: &str) -> Option<String> {
    let mut words = line.split_whitespace();
    words.find(|w| *w == "link/ether")?;
    ac_discovery::normalize_mac(words.next()?)
}

/// Fills battery and Wi-Fi fields from `STATS_CMD` output.
pub fn parse_stats(out: &str, stats: &mut DeviceStats) {
    for (name, lines) in sections(out) {
        match name {
            "battery" => parse_battery(&lines, stats),
            "wifi" => parse_wifi(&lines, stats),
            _ => {}
        }
    }
}

fn parse_battery(lines: &[&str], stats: &mut DeviceStats) {
    let mut powered = false;
    let mut status_charging = false;
    for line in lines {
        let Some((k, v)) = line.trim().split_once(':') else { continue };
        let v = v.trim();
        match k.trim() {
            "level" => stats.battery_pct = v.parse::<u8>().ok().map(|l| l.min(100)),
            "AC powered" | "USB powered" | "Wireless powered" | "Dock powered" => powered |= v == "true",
            // BatteryManager.BATTERY_STATUS_CHARGING = 2, FULL = 5
            "status" => status_charging = v == "2" || v == "5",
            _ => {}
        }
    }
    stats.charging = powered || status_charging;
}

fn parse_wifi(lines: &[&str], stats: &mut DeviceStats) {
    let text = lines.join(" ");
    stats.wifi_rssi_dbm = number_after(&text, &["RSSI: ", "rssi="]).and_then(|v| i16::try_from(v).ok());
    stats.wifi_link_mbps = number_after(&text, &["Link speed: ", "LinkSpeed: "]).and_then(|v| u32::try_from(v).ok());
}

fn number_after(text: &str, keys: &[&str]) -> Option<i64> {
    let lower = text.to_ascii_lowercase();
    keys.iter().find_map(|k| {
        let idx = lower.find(&k.to_ascii_lowercase())? + k.len();
        let rest = &text[idx..];
        let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '-')).unwrap_or(rest.len());
        rest[..end].parse().ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_identity() {
        let out = "@serial\n28291FDH200A1Z\n@model\nPixel 8\n@manufacturer\nGoogle\n@sdk\n35\n@aid\n9774d56d682e549c\n@mac\n\
31: wlan0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc mq state UP mode DORMANT group default qlen 3000\\    link/ether A6:3B:91:0C:4E:12 brd ff:ff:ff:ff:ff:ff\n";
        let id = parse_identity(out).unwrap();
        assert_eq!(id.id, "28291FDH200A1Z");
        assert_eq!(id.model, "Pixel 8");
        assert_eq!(id.manufacturer, "Google");
        assert_eq!(id.sdk, 35);
        assert_eq!(id.wifi_mac.as_deref(), Some("a6:3b:91:0c:4e:12"));
    }

    #[test]
    fn falls_back_to_android_id() {
        let out = "@serial\nunknown\n@model\nX\n@sdk\n29\n@aid\n9774d56d682e549c\n@mac\n";
        let id = parse_identity(out).unwrap();
        assert_eq!(id.id, "aid:9774d56d682e549c");
        assert_eq!(id.wifi_mac, None);
        assert!(parse_identity("@serial\n\n@aid\nnull\n").is_none());
    }

    #[test]
    fn parses_battery_and_wifi_status() {
        let out = "@battery\nCurrent Battery Service state:\n  AC powered: false\n  USB powered: false\n  Wireless powered: false\n  status: 3\n  level: 81\n  scale: 100\n\
@wifi\nWifiInfo: SSID: \"home\", BSSID: aa:bb, MAC: 02:00, Security type: 2, Supplicant state: COMPLETED, Wi-Fi standard: 11ax, RSSI: -58, Link speed: 1201Mbps, Tx Link speed: 1201Mbps\n";
        let mut s = DeviceStats::default();
        parse_stats(out, &mut s);
        assert_eq!(s.battery_pct, Some(81));
        assert!(!s.charging);
        assert_eq!(s.wifi_rssi_dbm, Some(-58));
        assert_eq!(s.wifi_link_mbps, Some(1201));
        assert_eq!(s.wifi_bars(), Some(3));
    }

    #[test]
    fn parses_dumpsys_wifi_fallback_and_charging() {
        let out = "@battery\n  USB powered: true\n  level: 100\n@wifi\n  mWifiInfo SSID: \"x\", rssi=-71, LinkSpeed: 433Mbps\n";
        let mut s = DeviceStats::default();
        parse_stats(out, &mut s);
        assert!(s.charging);
        assert_eq!(s.battery_pct, Some(100));
        assert_eq!(s.wifi_rssi_dbm, Some(-71));
        assert_eq!(s.wifi_link_mbps, Some(433));
    }

    #[test]
    fn missing_sections_leave_fields_empty() {
        let mut s = DeviceStats::default();
        parse_stats("@battery\n@wifi\n", &mut s);
        assert_eq!(s, DeviceStats::default());
    }
}
