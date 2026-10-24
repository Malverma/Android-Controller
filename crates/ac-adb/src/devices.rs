//! Parsing of `host:devices-l` / `host:track-devices-l` listings.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceState {
    Device,
    Offline,
    Unauthorized,
    Authorizing,
    Connecting,
    NoPermissions,
    Other(String),
}

impl DeviceState {
    fn parse(s: &str) -> Self {
        match s {
            "device" => Self::Device,
            "offline" => Self::Offline,
            "unauthorized" => Self::Unauthorized,
            "authorizing" => Self::Authorizing,
            "connecting" => Self::Connecting,
            "no" => Self::NoPermissions,
            other => Self::Other(other.to_owned()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Device => "device",
            Self::Offline => "offline",
            Self::Unauthorized => "unauthorized",
            Self::Authorizing => "authorizing",
            Self::Connecting => "connecting",
            Self::NoPermissions => "no permissions",
            Self::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceEntry {
    /// ADB serial: `ip:port` for `host:connect` transports, the mDNS instance
    /// (`adb-XXX-yyy._adb-tls-connect._tcp`) for adb-auto-connected ones, or
    /// the USB serial.
    pub serial: String,
    pub state: DeviceState,
    /// `usb:` devpath when the transport is USB.
    pub usb: Option<String>,
    pub product: Option<String>,
    pub model: Option<String>,
    pub device: Option<String>,
    pub transport_id: Option<u64>,
}

impl DeviceEntry {
    pub fn is_usb(&self) -> bool {
        self.usb.is_some()
    }

    /// True for TCP transports (`ip:port` or mDNS-named TLS transports).
    pub fn is_network(&self) -> bool {
        !self.is_usb() && (self.serial.contains(':') || self.serial.contains("._adb-tls-connect."))
    }
}

/// Parses the long-format listing, one device per line:
/// `serial  state usb:1-2 product:x model:y device:z transport_id:3`
pub fn parse_device_list(text: &str) -> Vec<DeviceEntry> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<DeviceEntry> {
    let mut tokens = line.split_whitespace();
    let serial = tokens.next()?.to_owned();
    let state = DeviceState::parse(tokens.next()?);
    let mut entry = DeviceEntry {
        serial,
        state,
        usb: None,
        product: None,
        model: None,
        device: None,
        transport_id: None,
    };
    for token in tokens {
        let Some((key, value)) = token.split_once(':') else {
            continue; // e.g. "permissions" in "no permissions"
        };
        match key {
            "usb" => entry.usb = Some(value.to_owned()),
            "product" => entry.product = Some(value.to_owned()),
            "model" => entry.model = Some(value.replace('_', " ")),
            "device" => entry.device = Some(value.to_owned()),
            "transport_id" => entry.transport_id = value.parse().ok(),
            _ => {}
        }
    }
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_long_listing() {
        let text = "\
192.168.1.42:37115     device product:husky model:Pixel_8_Pro device:husky transport_id:3
R58M12ABCDE            unauthorized usb:1-2 transport_id:4
adb-28291FDH200A1Z-OpVSoG._adb-tls-connect._tcp device product:shiba model:Pixel_8 device:shiba transport_id:5
emulator-5554          offline transport_id:1
0123456789ABCDEF       no permissions (missing udev rules); see [http://developer.android.com/tools/device.html] usb:3-1
";
        let list = parse_device_list(text);
        assert_eq!(list.len(), 5);

        assert_eq!(list[0].serial, "192.168.1.42:37115");
        assert_eq!(list[0].state, DeviceState::Device);
        assert_eq!(list[0].model.as_deref(), Some("Pixel 8 Pro"));
        assert_eq!(list[0].transport_id, Some(3));
        assert!(list[0].is_network());

        assert_eq!(list[1].state, DeviceState::Unauthorized);
        assert_eq!(list[1].usb.as_deref(), Some("1-2"));
        assert!(list[1].is_usb() && !list[1].is_network());

        assert!(list[2].is_network());
        assert_eq!(list[3].state, DeviceState::Offline);
        assert_eq!(list[4].state, DeviceState::NoPermissions);
        assert_eq!(list[4].usb.as_deref(), Some("3-1"));
    }

    #[test]
    fn ignores_blank_lines() {
        assert!(parse_device_list("\n\n").is_empty());
    }
}
