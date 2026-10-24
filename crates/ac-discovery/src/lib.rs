//! Discovery of Android wireless-debugging endpoints on the LAN.

mod arp;
mod avahi;
mod lan;

use std::collections::BTreeMap;
use std::net::SocketAddr;

pub use arp::{find_ip_by_mac, find_mac_by_ip, parse_arp_table, normalize_mac};
pub use avahi::spawn_avahi;
pub use lan::is_lan;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ServiceKind {
    /// `_adb-tls-pairing._tcp`: a pairing dialog (code or QR) is open.
    Pairing,
    /// `_adb-tls-connect._tcp`: wireless debugging is on.
    Connect,
    /// `_adb._tcp`: some legacy ROMs/emulators after `adb tcpip`.
    Legacy,
    /// `_acdirect._tcp`: the Android helper's connection without ADB. TXT
    /// `id` is the device id, `pair=1` while "Pair with PC" is open.
    Direct,
}

impl ServiceKind {
    pub const ALL: [ServiceKind; 4] = [Self::Pairing, Self::Connect, Self::Legacy, Self::Direct];

    pub fn service_type(self) -> &'static str {
        match self {
            Self::Pairing => "_adb-tls-pairing._tcp",
            Self::Connect => "_adb-tls-connect._tcp",
            Self::Legacy => "_adb._tcp",
            Self::Direct => "_acdirect._tcp",
        }
    }

    pub fn from_service_type(t: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.service_type() == t.trim_end_matches('.'))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredDevice {
    pub instance: String,
    pub kind: ServiceKind,
    pub addr: SocketAddr,
    pub host: String,
    pub txt: BTreeMap<String, String>,
}

impl DiscoveredDevice {
    /// Device serial embedded in `adb-<serial>-<suffix>` instance names (for
    /// the helper's service: the device id in its TXT record).
    pub fn serial(&self) -> Option<&str> {
        match self.kind {
            ServiceKind::Direct => self.txt.get("id").map(String::as_str).filter(|s| !s.is_empty()),
            _ => serial_from_instance(&self.instance),
        }
    }

    /// The helper's "Pair with PC" window is open.
    pub fn pairing_open(&self) -> bool {
        self.kind == ServiceKind::Direct && self.txt.get("pair").is_some_and(|v| v == "1")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    Added(DiscoveredDevice),
    Removed { instance: String, kind: ServiceKind },
    /// Discovery could not start (e.g. avahi-daemon not running).
    Unavailable(String),
}

/// `adb-28291FDH200A1Z-OpVSoG` → `28291FDH200A1Z`. Also accepts the adb
/// transport form with a `._adb-tls-connect._tcp` suffix. QR pairing
/// instances (`studio-…`) carry no serial.
pub fn serial_from_instance(instance: &str) -> Option<&str> {
    let name = instance.split("._adb").next()?;
    let rest = name.strip_prefix("adb-")?;
    let (serial, suffix) = rest.rsplit_once('-')?;
    (!serial.is_empty() && !suffix.is_empty()).then_some(serial)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_serial_from_instance_names() {
        assert_eq!(serial_from_instance("adb-28291FDH200A1Z-OpVSoG"), Some("28291FDH200A1Z"));
        assert_eq!(
            serial_from_instance("adb-28291FDH200A1Z-OpVSoG._adb-tls-connect._tcp"),
            Some("28291FDH200A1Z")
        );
        assert_eq!(serial_from_instance("adb-R5-CT12-xYz123"), Some("R5-CT12"));
        assert_eq!(serial_from_instance("studio-a1b2c3d4e5"), None);
        assert_eq!(serial_from_instance("adb-nosuffix"), None);
        assert_eq!(serial_from_instance("adb--x"), None);
    }

    #[test]
    fn helper_service_carries_its_id() {
        let mut d = DiscoveredDevice {
            instance: "AC-LM-X120-1a2b".into(),
            kind: ServiceKind::Direct,
            addr: "192.168.0.50:27190".parse().unwrap(),
            host: String::new(),
            txt: [("id".to_owned(), "direct:1a2b".to_owned()), ("pair".to_owned(), "1".to_owned())].into(),
        };
        assert_eq!(d.serial(), Some("direct:1a2b"));
        assert!(d.pairing_open());
        d.txt.insert("pair".into(), "0".into());
        assert!(!d.pairing_open());
        assert_eq!(ServiceKind::from_service_type("_acdirect._tcp."), Some(ServiceKind::Direct));
    }

    #[test]
    fn maps_service_types() {
        assert_eq!(ServiceKind::from_service_type("_adb-tls-connect._tcp"), Some(ServiceKind::Connect));
        assert_eq!(ServiceKind::from_service_type("_adb-tls-pairing._tcp."), Some(ServiceKind::Pairing));
        assert_eq!(ServiceKind::from_service_type("_http._tcp"), None);
    }
}
