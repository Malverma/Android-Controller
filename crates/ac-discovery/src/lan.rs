use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// True for private and link-local addresses (spec §3.1 LAN-only filter):
/// 10/8, 172.16/12, 192.168/16, 169.254/16, fc00::/7, fe80::/10, plus
/// IPv4-mapped forms of those.
pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_lan_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_lan_v4(v4),
            None => is_lan_v6(v6),
        },
    }
}

fn is_lan_v4(ip: Ipv4Addr) -> bool {
    ip.is_private() || ip.is_link_local()
}

fn is_lan_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_addresses() {
        let cases = [
            ("10.0.0.1", true),
            ("10.255.255.255", true),
            ("172.16.0.1", true),
            ("172.31.255.254", true),
            ("172.15.0.1", false),
            ("172.32.0.1", false),
            ("192.168.1.42", true),
            ("192.169.0.1", false),
            ("169.254.10.10", true),
            ("127.0.0.1", false),
            ("8.8.8.8", false),
            ("100.64.0.1", false), // CGNAT is not "local"
            ("0.0.0.0", false),
            ("255.255.255.255", false),
            ("fd12:3456::1", true),
            ("fc00::1", true),
            ("fe80::1", true),
            ("febf::1", true),
            ("fec0::1", false),
            ("2001:db8::1", false),
            ("::1", false),
            ("::ffff:192.168.1.5", true),
            ("::ffff:8.8.8.8", false),
        ];
        for (addr, expected) in cases {
            assert_eq!(is_lan(addr.parse().unwrap()), expected, "{addr}");
        }
    }
}
