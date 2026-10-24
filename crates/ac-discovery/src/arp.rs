//! Kernel neighbour table lookup, used to find legacy (`adb tcpip`) devices
//! again after a DHCP address change (spec §3.5).

use std::net::Ipv4Addr;

/// Lowercase, colon-separated; `None` unless it is 6 hex octets.
pub fn normalize_mac(mac: &str) -> Option<String> {
    let octets: Vec<&str> = mac.trim().split([':', '-']).collect();
    if octets.len() != 6 || !octets.iter().all(|o| o.len() == 2 && o.chars().all(|c| c.is_ascii_hexdigit())) {
        return None;
    }
    Some(octets.join(":").to_ascii_lowercase())
}

/// Parses `/proc/net/arp`, skipping incomplete entries (flags 0x0) and
/// all-zero MACs. Returns (ip, mac, interface).
pub fn parse_arp_table(text: &str) -> Vec<(Ipv4Addr, String, String)> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let [ip, _hw_type, flags, mac, _mask, dev] = cols[..] else { return None };
            if flags == "0x0" {
                return None;
            }
            let mac = normalize_mac(mac)?;
            if mac == "00:00:00:00:00:00" {
                return None;
            }
            Some((ip.parse().ok()?, mac, dev.to_owned()))
        })
        .collect()
}

pub fn find_ip_by_mac(arp_table: &str, mac: &str) -> Option<Ipv4Addr> {
    let want = normalize_mac(mac)?;
    parse_arp_table(arp_table).into_iter().find(|(_, m, _)| *m == want).map(|(ip, _, _)| ip)
}

/// The MAC the kernel has for `ip` (the reverse of [`find_ip_by_mac`]).
pub fn find_mac_by_ip(arp_table: &str, ip: Ipv4Addr) -> Option<String> {
    parse_arp_table(arp_table).into_iter().find(|(i, _, _)| *i == ip).map(|(_, m, _)| m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "\
IP address       HW type     Flags       HW address            Mask     Device
192.168.1.1      0x1         0x2         aa:bb:cc:dd:ee:ff     *        wlan0
192.168.1.57     0x1         0x2         A6:3B:91:0C:4E:12     *        wlan0
192.168.1.99     0x1         0x0         00:00:00:00:00:00     *        wlan0
";

    #[test]
    fn parses_and_finds_by_mac() {
        let rows = parse_arp_table(TABLE);
        assert_eq!(rows.len(), 2);
        assert_eq!(find_ip_by_mac(TABLE, "a6-3b-91-0c-4e-12"), Some("192.168.1.57".parse().unwrap()));
        assert_eq!(find_ip_by_mac(TABLE, "11:22:33:44:55:66"), None);
        assert_eq!(find_ip_by_mac(TABLE, "garbage"), None);
        assert_eq!(find_mac_by_ip(TABLE, "192.168.1.57".parse().unwrap()).as_deref(), Some("a6:3b:91:0c:4e:12"));
        assert_eq!(find_mac_by_ip(TABLE, "192.168.1.99".parse().unwrap()), None, "incomplete entry");
    }

    #[test]
    fn normalizes_macs() {
        assert_eq!(normalize_mac(" A6:3B:91:0C:4E:12\n").as_deref(), Some("a6:3b:91:0c:4e:12"));
        assert_eq!(normalize_mac("a6:3b:91:0c:4e"), None);
        assert_eq!(normalize_mac("zz:3b:91:0c:4e:12"), None);
    }
}
