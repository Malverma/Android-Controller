//! Active search of the local subnets, for the Reconnect button (spec2 §4.2):
//! finds phones when mDNS cannot, for example on a router that filters
//! multicast. It only runs when the user asks for it.

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// The Wireless Debugging Keeper's locator port (helper 0.8).
pub const LOCATOR_PORT: u16 = 27193;
/// What the PC sends to the locator; it answers with one JSON line.
pub const LOCATOR_REQUEST: &[u8] = b"TBL1\n";
const LOCATOR_MAX_REPLY: u64 = 1024;
const LOCATOR_READ_TIMEOUT: Duration = Duration::from_millis(500);
/// Android picks the wireless debugging port from this range.
pub const ADB_PORT_RANGE: std::ops::RangeInclusive<u16> = 30000..=49999;
/// Interfaces that never lead to a phone.
const SKIP_IFACE_PREFIXES: [&str; 7] = ["lo", "docker", "br-", "veth", "virbr", "tailscale", "tun"];

/// A directly connected IPv4 network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalNet {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

/// The networks in `/proc/net/route` worth searching: directly connected, up,
/// not virtual, and each listed once however many interfaces reach it.
pub fn parse_routes(table: &str) -> Vec<LocalNet> {
    const RTF_UP: u32 = 0x1;
    let mut nets = BTreeSet::new();
    for line in table.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let [iface, dest, gateway, flags, _, _, _, mask, ..] = cols[..] else { continue };
        if SKIP_IFACE_PREFIXES.iter().any(|p| iface.starts_with(p)) {
            continue;
        }
        let (Some(dest), Some(gateway), Ok(flags), Some(mask)) =
            (route_ip(dest), route_ip(gateway), u32::from_str_radix(flags, 16), route_ip(mask))
        else {
            continue;
        };
        let mask = u32::from(mask);
        // Not a default route, and a real contiguous netmask.
        if flags & RTF_UP == 0 || !gateway.is_unspecified() || mask == 0 || mask.leading_ones() != mask.count_ones() {
            continue;
        }
        nets.insert(LocalNet { network: Ipv4Addr::from(u32::from(dest) & mask), prefix: mask.count_ones() as u8 });
    }
    nets.into_iter().collect()
}

/// `/proc/net/route` writes addresses as little-endian hex.
fn route_ip(hex: &str) -> Option<Ipv4Addr> {
    Some(Ipv4Addr::from(u32::from_str_radix(hex, 16).ok()?.swap_bytes()))
}

impl LocalNet {
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(self.prefix)) };
        u32::from(ip) & mask == u32::from(self.network) & mask
    }

    /// The hosts to try. A network larger than /24 is limited to the /24 around
    /// `own` (this PC's address), and skipped when that is not known.
    pub fn hosts(&self, own: Option<Ipv4Addr>) -> Vec<Ipv4Addr> {
        let (base, prefix) = if self.prefix >= 24 {
            (u32::from(self.network), self.prefix)
        } else {
            let Some(own) = own.filter(|ip| self.contains(*ip)) else { return Vec::new() };
            (u32::from(own) & 0xffff_ff00, 24)
        };
        let count = 1u32 << (32 - u32::from(prefix));
        if count <= 2 {
            return Vec::new();
        }
        (1..count - 1).map(|i| Ipv4Addr::from(base + i)).collect()
    }
}

/// True when `ip` is on one of `nets`. An empty list means the networks are not
/// known, so nothing can be ruled out.
pub fn on_local_net(nets: &[LocalNet], ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => nets.is_empty() || nets.iter().any(|n| n.contains(v4)),
        IpAddr::V6(_) => true,
    }
}

/// This PC's address on `net`. Connecting a UDP socket sends no packets.
pub fn own_address(net: &LocalNet) -> Option<Ipv4Addr> {
    let probe = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(u32::from(net.network) + 1)), 9);
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(probe).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if net.contains(ip) => Some(ip),
        _ => None,
    }
}

/// Every host to search on `nets`, without duplicates.
pub fn sweep_hosts(nets: &[LocalNet]) -> Vec<IpAddr> {
    let set: BTreeSet<Ipv4Addr> = nets.iter().flat_map(|n| n.hosts(own_address(n))).collect();
    set.into_iter().map(IpAddr::V4).collect()
}

/// The Keeper's answer on [`LOCATOR_PORT`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LocatorReply {
    pub keeper_id: String,
    #[serde(default)]
    pub version: u32,
    /// Wireless debugging is on.
    #[serde(default)]
    pub adb_on: bool,
    /// Its current port, when the helper could read it.
    #[serde(default)]
    pub adb_port: Option<u16>,
    /// Android trusts this Wi-Fi network for wireless debugging; `None` when
    /// the helper cannot tell.
    #[serde(default)]
    pub wifi_trusted: Option<bool>,
}

#[derive(Debug, Clone, Copy)]
pub struct SweepPlan {
    pub locator_port: u16,
    pub direct_port: u16,
    pub legacy_port: u16,
    pub concurrency: usize,
    pub timeout: Duration,
}

/// One host that answered on at least one port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub ip: IpAddr,
    pub locator: Option<LocatorReply>,
    pub direct_open: bool,
    pub legacy_open: bool,
}

async fn open(addr: SocketAddr, timeout: Duration) -> Option<TcpStream> {
    tokio::time::timeout(timeout, TcpStream::connect(addr)).await.ok()?.ok()
}

async fn ask_locator(addr: SocketAddr, timeout: Duration) -> Option<LocatorReply> {
    let mut s = open(addr, timeout).await?;
    let exchange = async {
        s.write_all(LOCATOR_REQUEST).await.ok()?;
        let mut reply = Vec::new();
        (&mut s).take(LOCATOR_MAX_REPLY).read_to_end(&mut reply).await.ok()?;
        serde_json::from_slice::<LocatorReply>(&reply).ok()
    };
    tokio::time::timeout(LOCATOR_READ_TIMEOUT, exchange).await.ok()?
}

/// Tries the helper's two ports and the `adb tcpip` port on every host.
/// `concurrency` limits hosts in flight, each making up to three connections.
pub async fn sweep(hosts: Vec<IpAddr>, plan: SweepPlan) -> Vec<Found> {
    let limit = Arc::new(Semaphore::new(plan.concurrency.max(1)));
    let mut tasks = JoinSet::new();
    for ip in hosts {
        let limit = limit.clone();
        tasks.spawn(async move {
            let _permit = limit.acquire_owned().await.ok()?;
            let (locator, direct, legacy) = tokio::join!(
                ask_locator(SocketAddr::new(ip, plan.locator_port), plan.timeout),
                open(SocketAddr::new(ip, plan.direct_port), plan.timeout),
                open(SocketAddr::new(ip, plan.legacy_port), plan.timeout),
            );
            let found = Found { ip, locator, direct_open: direct.is_some(), legacy_open: legacy.is_some() };
            (found.locator.is_some() || found.direct_open || found.legacy_open).then_some(found)
        });
    }
    let mut out = Vec::new();
    while let Some(done) = tasks.join_next().await {
        out.extend(done.ok().flatten());
    }
    out.sort_by_key(|f| f.ip);
    out
}

/// Finds open TCP ports on one host: the last resort when a phone's address is
/// known but not its wireless debugging port.
pub async fn port_search(
    ip: IpAddr,
    ports: std::ops::RangeInclusive<u16>,
    concurrency: usize,
    timeout: Duration,
) -> Vec<u16> {
    let limit = Arc::new(Semaphore::new(concurrency.max(1)));
    let mut tasks = JoinSet::new();
    for port in ports {
        let limit = limit.clone();
        tasks.spawn(async move {
            let _permit = limit.acquire_owned().await.ok()?;
            open(SocketAddr::new(ip, port), timeout).await.map(|_| port)
        });
    }
    let mut out = Vec::new();
    while let Some(done) = tasks.join_next().await {
        out.extend(done.ok().flatten());
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// The real routing table of the dual-homed laptop this was built on.
    const ROUTES: &str = "\
Iface\tDestination\tGateway\tFlags\tRefCnt\tUse\tMetric\tMask\tMTU\tWindow\tIRTT
enp0s31f6\t00000000\t0101A8C0\t0003\t0\t0\t100\t00000000\t0\t0\t0
wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0
enp0s31f6\t0001A8C0\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
wlan0\t0001A8C0\t00000000\t0001\t0\t0\t600\t00FFFFFF\t0\t0\t0
enp0s31f6\t0101A8C0\t00000000\t0005\t0\t0\t100\tFFFFFFFF\t0\t0\t0
";

    #[test]
    fn two_interfaces_on_one_subnet_scan_it_once_and_skip_docker() {
        let nets = parse_routes(ROUTES);
        assert!(nets.contains(&LocalNet { network: Ipv4Addr::new(192, 168, 1, 0), prefix: 24 }));
        assert!(!nets.iter().any(|n| n.network == Ipv4Addr::new(172, 17, 0, 0)), "{nets:?}");
        assert_eq!(nets.iter().filter(|n| n.prefix == 24).count(), 1, "{nets:?}");
        // The gateway's host route adds no hosts.
        assert!(nets.iter().filter(|n| n.prefix == 32).all(|n| n.hosts(None).is_empty()));
    }

    #[test]
    fn ignores_down_routes_and_bad_lines() {
        let text = "header\nwlan0\t0001A8C0\t00000000\t0000\t0\t0\t0\t00FFFFFF\t0\t0\t0\nshort line\n";
        assert!(parse_routes(text).is_empty());
    }

    #[test]
    fn host_lists() {
        let net = LocalNet { network: Ipv4Addr::new(192, 168, 1, 0), prefix: 24 };
        let hosts = net.hosts(None);
        assert_eq!(hosts.len(), 254);
        assert_eq!(hosts[0], Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(hosts[253], Ipv4Addr::new(192, 168, 1, 254));

        // A /16 is limited to the /24 around this PC, or skipped without it.
        let wide = LocalNet { network: Ipv4Addr::new(10, 1, 0, 0), prefix: 16 };
        assert!(wide.hosts(None).is_empty());
        let own = Ipv4Addr::new(10, 1, 7, 9);
        let hosts = wide.hosts(Some(own));
        assert_eq!((hosts[0], hosts.len()), (Ipv4Addr::new(10, 1, 7, 1), 254));
        assert!(wide.hosts(Some(Ipv4Addr::new(10, 2, 7, 9))).is_empty(), "own address off this network");
    }

    #[test]
    fn local_net_check() {
        let nets = [LocalNet { network: Ipv4Addr::new(192, 168, 1, 0), prefix: 24 }];
        assert!(on_local_net(&nets, "192.168.1.84".parse().unwrap()));
        assert!(!on_local_net(&nets, "192.168.0.84".parse().unwrap()));
        assert!(on_local_net(&[], "192.168.0.84".parse().unwrap()), "unknown networks rule nothing out");
    }

    async fn locator_server(reply: &'static str) -> u16 {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 5];
                if s.read_exact(&mut buf).await.is_ok() && buf == *LOCATOR_REQUEST {
                    let _ = s.write_all(reply.as_bytes()).await;
                }
            }
        });
        port
    }

    fn plan(locator_port: u16, direct_port: u16, legacy_port: u16) -> SweepPlan {
        SweepPlan { locator_port, direct_port, legacy_port, concurrency: 8, timeout: Duration::from_millis(300) }
    }

    /// A port that is closed: bound to find a free number, then released.
    async fn closed_port() -> u16 {
        TcpListener::bind(("127.0.0.1", 0)).await.unwrap().local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn sweep_reads_the_locator_and_reports_open_ports() {
        let locator = locator_server(r#"{"keeper_id":"k1","version":8,"adb_on":true,"adb_port":41927,"wifi_trusted":true}"#).await;
        let direct = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let closed = closed_port().await;
        let host: IpAddr = "127.0.0.1".parse().unwrap();

        let found = sweep(vec![host], plan(locator, direct.local_addr().unwrap().port(), closed)).await;
        assert_eq!(found.len(), 1);
        let reply = found[0].locator.as_ref().unwrap();
        assert_eq!((reply.keeper_id.as_str(), reply.adb_port, reply.wifi_trusted), ("k1", Some(41927), Some(true)));
        assert!(found[0].direct_open);
        assert!(!found[0].legacy_open);
    }

    #[tokio::test]
    async fn sweep_omits_silent_hosts_and_survives_a_bad_reply() {
        let closed = closed_port().await;
        let host: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(sweep(vec![host], plan(closed, closed, closed)).await.is_empty());

        // Something else on the port, not speaking the protocol.
        let junk = locator_server("not json").await;
        assert!(sweep(vec![host], plan(junk, closed, closed)).await.is_empty());
    }

    #[tokio::test]
    async fn locator_reply_without_a_port_is_accepted() {
        let locator = locator_server(r#"{"keeper_id":"k2","adb_on":false}"#).await;
        let closed = closed_port().await;
        let found = sweep(vec!["127.0.0.1".parse().unwrap()], plan(locator, closed, closed)).await;
        let reply = found[0].locator.as_ref().unwrap();
        assert_eq!((reply.adb_on, reply.adb_port, reply.wifi_trusted), (false, None, None));
    }

    #[tokio::test]
    async fn port_search_finds_only_open_ports() {
        let l = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = l.local_addr().unwrap().port();
        let from = port.saturating_sub(3);
        let to = port.saturating_add(3);
        let open = port_search("127.0.0.1".parse().unwrap(), from..=to, 16, Duration::from_millis(300)).await;
        assert!(open.contains(&port), "{open:?}");
    }
}
