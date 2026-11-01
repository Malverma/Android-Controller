//! The Reconnect button (spec2 §4): finds saved phones again after their
//! addresses changed, then lets the slot state machine connect and verify them.
//!
//! Each phone gets a queue of addresses to try, best first, fed by mDNS and by
//! an active search of the local subnets. Trying a wrong address is safe: the
//! slot checks the identity of whatever answers and rejects any other phone.

use std::collections::{HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ac_core::{SlotId, TransportKind};
use ac_discovery::ServiceKind;
use ac_discovery::sweep::{self, Found, LOCATOR_PORT, SweepPlan};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::Msg;
use super::automation::Reply;
use crate::api::{ReconnectResult, UiEvent};
use crate::manager::Manager;
use crate::slot::{Phase, SlotEvent};

const SHELL_PROBE: &str = "echo ac";
const STEP_SEARCHING: &str = "Searching the network…";

/// Timings and ports of the search. The defaults are for real networks.
#[derive(Debug, Clone)]
pub struct ReconnectOptions {
    pub route_table: PathBuf,
    /// Hosts to search instead of the local subnets (tests).
    pub hosts: Option<Vec<IpAddr>>,
    pub locator_port: u16,
    pub direct_port: u16,
    /// How long mDNS gets before the active search starts.
    pub mdns_wait: Duration,
    /// The whole job; slower phones still connect through normal discovery.
    pub deadline: Duration,
    /// Time an address gets before the next one is tried.
    pub try_gap: Duration,
    pub tick: Duration,
    pub online_probe_timeout: Duration,
    pub sweep_concurrency: usize,
    pub sweep_timeout: Duration,
    pub adb_ports: std::ops::RangeInclusive<u16>,
    pub port_search_concurrency: usize,
    pub port_search_timeout: Duration,
}

impl Default for ReconnectOptions {
    fn default() -> Self {
        Self {
            route_table: "/proc/net/route".into(),
            hosts: None,
            locator_port: LOCATOR_PORT,
            direct_port: ac_direct::PORT,
            mdns_wait: Duration::from_secs(4),
            deadline: Duration::from_secs(20),
            try_gap: Duration::from_millis(1500),
            tick: Duration::from_millis(250),
            online_probe_timeout: Duration::from_millis(1500),
            sweep_concurrency: 64,
            sweep_timeout: Duration::from_millis(300),
            adb_ports: sweep::ADB_PORT_RANGE,
            port_search_concurrency: 256,
            port_search_timeout: Duration::from_millis(150),
        }
    }
}

pub enum JobMsg {
    Tick { job: u64 },
    OnlineProbe { job: u64, slot: SlotId, serial: String, alive: bool },
    Swept { job: u64, found: Vec<Found>, arp: String },
    PortsFound { job: u64, slot: SlotId, ip: IpAddr, ports: Vec<u16> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    addr: String,
    transport: TransportKind,
}

struct Target {
    slot: SlotId,
    title: String,
    step: String,
    queue: VecDeque<Candidate>,
    tried: HashSet<String>,
    last_try: Option<Instant>,
    /// Certainly this phone's address (its mDNS name, Keeper id or Wi-Fi MAC).
    found_ip: Option<IpAddr>,
    port_search_started: bool,
    /// The Keeper reported wireless debugging off on the phone.
    debugging_off: bool,
    ok: bool,
}

impl Target {
    fn offer(&mut self, c: Candidate, best: bool) {
        if best {
            // Fresh and certain: worth trying again, and right now.
            self.tried.remove(&c.addr);
            self.queue.retain(|q| q.addr != c.addr);
            self.queue.push_front(c);
            self.last_try = None;
        } else if !self.tried.contains(&c.addr) && !self.queue.contains(&c) {
            self.queue.push_back(c);
        }
    }
}

pub struct ReconnectJob {
    id: u64,
    started: Instant,
    targets: Vec<Target>,
    probes_pending: usize,
    sweep_started: bool,
    swept: bool,
    ports_pending: usize,
    replies: Vec<Reply>,
    ticker: JoinHandle<()>,
}

impl Manager {
    /// The search step to show on a phone's tile.
    pub(super) fn reconnect_step(&self, slot: SlotId) -> Option<&str> {
        let t = self.reconnect_job.as_ref()?.targets.iter().find(|t| t.slot == slot && !t.ok)?;
        Some(t.step.as_str())
    }

    pub(super) fn start_reconnect(&mut self, only: Option<SlotId>) {
        self.start_reconnect_with(only, None);
    }

    /// `reply` answers an automation client when the search ends.
    pub(super) fn start_reconnect_with(&mut self, only: Option<SlotId>, reply: Option<Reply>) {
        if let Some(job) = &mut self.reconnect_job {
            // A second click does nothing; a script just waits for the result.
            job.replies.extend(reply);
            return;
        }
        let mut wanted = Vec::new();
        let mut online = Vec::new();
        for s in &self.slots {
            if s.device_id().is_none() || only.is_some_and(|o| o != s.id) {
                continue;
            }
            match s.phase {
                Phase::Disconnected { .. } | Phase::Reconnecting { .. } | Phase::ConnectingAdb { .. } => wanted.push(s.id),
                Phase::Online { .. } if s.transport != TransportKind::Direct => {
                    // It looks online; the search makes sure of it.
                    let shell_v2 = s.device.as_ref().is_some_and(|d| d.shell_v2());
                    online.extend(s.adb_serial.clone().map(|serial| (s.id, serial, shell_v2)));
                }
                _ => {}
            }
        }
        if wanted.is_empty() && online.is_empty() {
            self.send(UiEvent::ReconnectDone { results: Vec::new() });
            if let Some(r) = reply {
                let _ = r.send(Ok(json!({ "results": [] })));
            }
            return;
        }

        self.reconnect_seq += 1;
        let id = self.reconnect_seq;
        let every = self.reconnect_opts.tick;
        let tx = self.tx.clone();
        let ticker = tokio::spawn(async move {
            let mut t = tokio::time::interval(every);
            loop {
                t.tick().await;
                if tx.send(Msg::Reconnect(JobMsg::Tick { job: id })).is_err() {
                    return;
                }
            }
        });
        self.reconnect_job = Some(ReconnectJob {
            id,
            started: Instant::now(),
            targets: Vec::new(),
            probes_pending: online.len(),
            sweep_started: false,
            swept: false,
            ports_pending: 0,
            replies: reply.into_iter().collect(),
            ticker,
        });
        self.send(UiEvent::Reconnecting { active: true });

        for slot in wanted {
            self.add_target(slot);
        }
        for (slot, serial, shell_v2) in online {
            let adb = self.adb.clone();
            let limit = self.reconnect_opts.online_probe_timeout;
            self.spawn_msg(async move {
                let answer = tokio::time::timeout(limit, adb.shell(&serial, SHELL_PROBE, shell_v2)).await;
                let alive = matches!(answer, Ok(Ok(_)));
                Msg::Reconnect(JobMsg::OnlineProbe { job: id, slot, serial, alive })
            });
        }
        // What is already known, then fresh answers from the network.
        let known: Vec<_> =
            self.nearby.values().filter(|n| n.stale_since.is_none()).map(|n| n.dev.clone()).collect();
        for dev in known {
            match (dev.kind, dev.serial()) {
                (ServiceKind::Connect, Some(serial)) => {
                    self.reconnect_offer_connect(serial, dev.addr);
                }
                (ServiceKind::Direct, Some(id)) if self.keys.is_paired(id) => {
                    self.reconnect_offer_direct(id, dev.addr);
                }
                _ => {}
            }
        }
        for kind in ServiceKind::ALL {
            self.rescan(kind);
        }
        self.reconnect_tick();
    }

    fn add_target(&mut self, slot: SlotId) {
        let Some(i) = self.idx(slot) else { return };
        let s = &self.slots[i];
        let id = s.device_id().map(str::to_owned);
        let saved = s.addr.clone();
        let (transport, legacy) = (s.transport, s.legacy_addr.clone());
        let title = self.title(i);
        if let Some(id) = &id {
            self.suppressed.remove(id);
        }
        if let Some(a) = &saved {
            self.suppressed.remove(a);
        }
        let mut target = Target {
            slot,
            title,
            step: STEP_SEARCHING.to_owned(),
            queue: VecDeque::new(),
            tried: HashSet::new(),
            last_try: None,
            found_ip: None,
            port_search_started: false,
            debugging_off: false,
            ok: false,
        };
        // Its saved address may still work, unless no route leads there.
        let nets = std::fs::read_to_string(&self.reconnect_opts.route_table).map(|t| sweep::parse_routes(&t)).unwrap_or_default();
        let reachable = |a: &String| a.parse::<SocketAddr>().is_ok_and(|sa| sweep::on_local_net(&nets, sa.ip()));
        if let Some(a) = saved.filter(reachable) {
            target.offer(Candidate { addr: a, transport }, false);
        }
        if let Some(a) = legacy.filter(reachable) {
            target.offer(Candidate { addr: a, transport: TransportKind::Legacy }, false);
        }
        if let Some(job) = &mut self.reconnect_job {
            job.targets.push(target);
        }
        self.feed(slot, SlotEvent::SearchStarted);
    }

    /// The user disconnected or forgot this phone: stop looking for it.
    pub(super) fn reconnect_drop_target(&mut self, slot: SlotId) {
        if let Some(job) = &mut self.reconnect_job {
            job.targets.retain(|t| t.slot != slot);
        }
    }

    fn target_for_device(&mut self, device_id: &str) -> Option<&mut Target> {
        let slot = self.slots.iter().find(|s| s.device_id() == Some(device_id) || s.expect_serial.as_deref() == Some(device_id))?.id;
        self.reconnect_job.as_mut()?.targets.iter_mut().find(|t| t.slot == slot && !t.ok)
    }

    /// mDNS named this phone. Returns true when the search takes it from here.
    pub(super) fn reconnect_offer_connect(&mut self, serial: &str, addr: SocketAddr) -> bool {
        self.reconnect_offer(serial, addr, TransportKind::Tls)
    }

    pub(super) fn reconnect_offer_direct(&mut self, id: &str, addr: SocketAddr) -> bool {
        self.reconnect_offer(id, addr, TransportKind::Direct)
    }

    fn reconnect_offer(&mut self, device_id: &str, addr: SocketAddr, transport: TransportKind) -> bool {
        let Some(t) = self.target_for_device(device_id) else { return false };
        t.found_ip = Some(addr.ip());
        t.offer(Candidate { addr: addr.to_string(), transport }, true);
        self.reconnect_tick();
        true
    }

    pub(super) fn on_reconnect_msg(&mut self, msg: JobMsg) {
        let Some(current) = self.reconnect_job.as_ref().map(|j| j.id) else { return };
        match msg {
            JobMsg::Tick { job } if job == current => self.reconnect_tick(),
            JobMsg::OnlineProbe { job, slot, serial, alive } if job == current => {
                if let Some(j) = &mut self.reconnect_job {
                    j.probes_pending = j.probes_pending.saturating_sub(1);
                }
                let still_online = self.idx(slot).is_some_and(|i| matches!(self.slots[i].phase, Phase::Online { .. }));
                if !alive && still_online {
                    self.feed(slot, SlotEvent::AdbGone { serial });
                    self.add_target(slot);
                }
                self.reconnect_tick();
            }
            JobMsg::Swept { job, found, arp } if job == current => {
                if let Some(j) = &mut self.reconnect_job {
                    j.swept = true;
                }
                self.assign_found(&found, &arp);
                self.reconnect_tick();
            }
            JobMsg::PortsFound { job, slot, ip, ports } if job == current => {
                if let Some(j) = &mut self.reconnect_job {
                    j.ports_pending = j.ports_pending.saturating_sub(1);
                    if let Some(t) = j.targets.iter_mut().find(|t| t.slot == slot) {
                        for port in ports {
                            let addr = SocketAddr::new(ip, port).to_string();
                            t.offer(Candidate { addr, transport: TransportKind::Tls }, false);
                        }
                    }
                }
                self.reconnect_tick();
            }
            _ => {} // from an earlier search
        }
    }

    /// Turns what the sweep found into addresses to try for each phone.
    fn assign_found(&mut self, found: &[Found], arp: &str) {
        let legacy_port = self.cfg.discovery.legacy_port;
        let direct_port = self.reconnect_opts.direct_port;
        let wanted: Vec<SlotId> = self.reconnect_job.as_ref().map_or_else(Vec::new, |j| j.targets.iter().filter(|t| !t.ok).map(|t| t.slot).collect());
        for slot in wanted {
            let Some(i) = self.idx(slot) else { continue };
            let s = &self.slots[i];
            let rec = s.device_id().and_then(|id| self.devices.get(id));
            let keeper = rec.and_then(|r| r.keeper_id.clone());
            let mac = s.wifi_mac.clone().or_else(|| rec.and_then(|r| r.wifi_mac.clone()));
            let plan = PlanInputs { keeper, mac, transport: s.transport, has_legacy: s.legacy_addr.is_some(), legacy_port, direct_port };
            let Some(t) = self.reconnect_job.as_mut().and_then(|j| j.targets.iter_mut().find(|t| t.slot == slot)) else { continue };
            for f in found {
                let c = plan_candidates(&plan, f, arp);
                if c.certain {
                    t.found_ip = Some(f.ip);
                    t.debugging_off |= f.locator.as_ref().is_some_and(|l| !l.adb_on);
                }
                for candidate in c.addrs {
                    t.offer(candidate, c.certain);
                }
            }
        }
    }

    pub(super) fn reconnect_tick(&mut self) {
        let Some(job) = self.reconnect_job.as_ref() else { return };
        let opts = self.reconnect_opts.clone();
        let elapsed = job.started.elapsed();

        // Who is back, and who gets another address to try.
        let mut tries = Vec::new();
        let mut searches = Vec::new();
        let in_use: HashSet<String> = self
            .slots
            .iter()
            .filter(|s| matches!(s.phase, Phase::Online { .. }))
            .flat_map(|s| s.adb_serial.iter().chain(s.addr.iter()).cloned())
            .collect();
        let online: HashSet<SlotId> = self.slots.iter().filter(|s| matches!(s.phase, Phase::Online { .. })).map(|s| s.id).collect();
        let identifying: HashSet<SlotId> =
            self.slots.iter().filter(|s| matches!(s.phase, Phase::ConnectingAdb { connected: true, .. })).map(|s| s.id).collect();
        let job = self.reconnect_job.as_mut().expect("checked above");
        let swept = job.swept;
        for t in &mut job.targets {
            if online.contains(&t.slot) {
                t.ok = true;
                continue;
            }
            // A phone that accepted the connection gets longer to identify itself.
            let gap = if identifying.contains(&t.slot) { opts.try_gap * 3 } else { opts.try_gap };
            if t.last_try.is_some_and(|at| at.elapsed() < gap) {
                continue;
            }
            let mut next = None;
            while let Some(c) = t.queue.pop_front() {
                if !in_use.contains(&c.addr) {
                    next = Some(c);
                    break;
                }
            }
            match next {
                Some(c) => {
                    t.tried.insert(c.addr.clone());
                    t.last_try = Some(Instant::now());
                    t.step = match c.addr.rsplit_once(':') {
                        Some((ip, _)) => format!("Trying {ip}…"),
                        None => format!("Trying {}…", c.addr),
                    };
                    tries.push((t.slot, c));
                }
                None => {
                    // Last resort: its address is known, its port is not.
                    if swept
                        && !t.port_search_started
                        && !t.debugging_off
                        && let Some(ip) = t.found_ip
                    {
                        t.port_search_started = true;
                        t.step = "Looking for its wireless debugging port…".to_owned();
                        searches.push((t.slot, ip));
                    }
                }
            }
        }
        job.ports_pending += searches.len();

        let start_sweep = !job.sweep_started && elapsed >= opts.mdns_wait && job.targets.iter().any(|t| !t.ok);
        job.sweep_started |= start_sweep;
        let id = job.id;
        let done = job.probes_pending == 0 && job.targets.iter().all(|t| t.ok);
        let timed_out = elapsed >= opts.deadline;

        for (slot, c) in tries {
            self.feed(slot, SlotEvent::Relocate { addr: c.addr, transport: c.transport });
        }
        for (slot, ip) in searches {
            let (ports, width, limit) = (opts.adb_ports.clone(), opts.port_search_concurrency, opts.port_search_timeout);
            self.spawn_msg(async move {
                let ports = sweep::port_search(ip, ports, width, limit).await;
                Msg::Reconnect(JobMsg::PortsFound { job: id, slot, ip, ports })
            });
        }
        if start_sweep {
            self.start_sweep(id, &opts);
        }
        if done || timed_out {
            self.finish_reconnect();
        }
    }

    fn start_sweep(&self, job: u64, opts: &ReconnectOptions) {
        let plan = SweepPlan {
            locator_port: opts.locator_port,
            direct_port: opts.direct_port,
            legacy_port: self.cfg.discovery.legacy_port,
            concurrency: opts.sweep_concurrency,
            timeout: opts.sweep_timeout,
        };
        let (hosts, routes, arp_path) = (opts.hosts.clone(), opts.route_table.clone(), self.arp_table.clone());
        self.spawn_msg(async move {
            let hosts = match hosts {
                Some(h) => h,
                None => {
                    let table = tokio::fs::read_to_string(&routes).await.unwrap_or_default();
                    let nets = sweep::parse_routes(&table);
                    tokio::task::spawn_blocking(move || sweep::sweep_hosts(&nets)).await.unwrap_or_default()
                }
            };
            let found = sweep::sweep(hosts, plan).await;
            // Connecting just filled the kernel's neighbour table.
            let arp = tokio::fs::read_to_string(&arp_path).await.unwrap_or_default();
            Msg::Reconnect(JobMsg::Swept { job, found, arp })
        });
    }

    fn finish_reconnect(&mut self) {
        let Some(job) = self.reconnect_job.take() else { return };
        job.ticker.abort();
        let mut results = Vec::new();
        for t in &job.targets {
            let at = self.idx(t.slot).and_then(|i| self.slots[i].addr.clone());
            let (ok, message) = if t.ok {
                (true, format!("Reconnected at {}", at.unwrap_or_default()))
            } else {
                (false, failure_message(t))
            };
            if !ok {
                // A phone still in `Reconnecting` keeps trying on its own.
                self.feed(t.slot, SlotEvent::SearchFailed(message.clone()));
            }
            results.push(ReconnectResult { slot: t.slot, title: t.title.clone(), ok, message });
        }
        for reply in job.replies {
            let _ = reply.send(Ok(results_json(&results)));
        }
        self.send(UiEvent::Reconnecting { active: false });
        self.send(UiEvent::ReconnectDone { results });
    }
}

fn failure_message(t: &Target) -> String {
    let name = &t.title;
    if t.debugging_off {
        return format!("On {name}: turn on Wireless debugging once and tick “Always allow on this network”");
    }
    if let Some(ip) = t.found_ip {
        return format!("Found {name} at {ip}, but it would not connect. If it was paired with another PC, pair it with this one");
    }
    format!("Could not find {name}. It may be off or out of range; if it is on, use Add Device and type its IP:port")
}

fn results_json(results: &[ReconnectResult]) -> Value {
    let rows: Vec<Value> =
        results.iter().map(|r| json!({ "slot": r.slot, "phone": r.title, "ok": r.ok, "message": r.message })).collect();
    json!({ "results": rows })
}

struct PlanInputs {
    keeper: Option<String>,
    mac: Option<String>,
    transport: TransportKind,
    has_legacy: bool,
    legacy_port: u16,
    direct_port: u16,
}

#[derive(Debug, PartialEq, Eq)]
struct Planned {
    /// The host is this phone, going by its Keeper id or Wi-Fi MAC.
    certain: bool,
    addrs: Vec<Candidate>,
}

/// What to try on one host for one phone. Pure, so it can be tested. Uncertain
/// addresses are still worth trying, since the slot verifies the identity.
fn plan_candidates(p: &PlanInputs, f: &Found, arp: &str) -> Planned {
    let nothing = Planned { certain: false, addrs: Vec::new() };
    let locator = f.locator.as_ref();
    let keeper_match = match (&p.keeper, locator) {
        (Some(k), Some(l)) if *k == l.keeper_id => true,
        // A Keeper that says it is a different phone certainly is.
        (Some(_), Some(_)) => return nothing,
        _ => false,
    };
    let host_mac = match f.ip {
        IpAddr::V4(ip) => ac_discovery::find_mac_by_ip(arp, ip),
        IpAddr::V6(_) => None,
    };
    let mac_match = match (&p.mac, &host_mac) {
        (Some(want), Some(have)) => ac_discovery::normalize_mac(want).as_deref() == Some(have.as_str()),
        _ => false,
    };
    let certain = keeper_match || mac_match;
    let mut addrs = Vec::new();
    if let Some(l) = locator
        && l.adb_on
        && let Some(port) = l.adb_port
        && p.transport != TransportKind::Direct
    {
        addrs.push(Candidate { addr: SocketAddr::new(f.ip, port).to_string(), transport: TransportKind::Tls });
    }
    if f.legacy_open && (certain || p.transport == TransportKind::Legacy || p.has_legacy) {
        addrs.push(Candidate { addr: SocketAddr::new(f.ip, p.legacy_port).to_string(), transport: TransportKind::Legacy });
    }
    if f.direct_open && p.transport == TransportKind::Direct {
        addrs.push(Candidate { addr: SocketAddr::new(f.ip, p.direct_port).to_string(), transport: TransportKind::Direct });
    }
    Planned { certain, addrs }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ac_discovery::sweep::LocatorReply;

    const ARP: &str = "IP address HW type Flags HW address Mask Device\n\
192.168.1.84 0x1 0x2 a6:3b:91:0c:4e:12 * wlan0\n";

    fn inputs(transport: TransportKind) -> PlanInputs {
        PlanInputs { keeper: None, mac: None, transport, has_legacy: false, legacy_port: 5555, direct_port: 27190 }
    }

    fn host() -> Found {
        Found { ip: "192.168.1.84".parse().unwrap(), locator: None, direct_open: false, legacy_open: false }
    }

    fn keeper(id: &str, port: Option<u16>) -> Option<LocatorReply> {
        Some(LocatorReply { keeper_id: id.into(), version: 8, adb_on: true, adb_port: port, wifi_trusted: Some(true) })
    }

    #[test]
    fn matching_keeper_is_certain_and_gives_the_port() {
        let p = PlanInputs { keeper: Some("k1".into()), ..inputs(TransportKind::Tls) };
        let got = plan_candidates(&p, &Found { locator: keeper("k1", Some(41927)), ..host() }, ARP);
        assert!(got.certain);
        assert_eq!(got.addrs, vec![Candidate { addr: "192.168.1.84:41927".into(), transport: TransportKind::Tls }]);
    }

    #[test]
    fn another_keeper_is_never_tried() {
        let p = PlanInputs { keeper: Some("k1".into()), ..inputs(TransportKind::Tls) };
        let f = Found { locator: keeper("other", Some(41927)), legacy_open: true, ..host() };
        assert_eq!(plan_candidates(&p, &f, ARP), Planned { certain: false, addrs: vec![] });
    }

    #[test]
    fn saved_mac_makes_a_host_certain_even_without_a_port() {
        let p = PlanInputs { mac: Some("A6-3B-91-0C-4E-12".into()), ..inputs(TransportKind::Tls) };
        let got = plan_candidates(&p, &host(), ARP);
        assert!(got.certain, "the port search takes over from here");
        assert!(got.addrs.is_empty());
    }

    #[test]
    fn unknown_keeper_is_tried_but_not_trusted() {
        // Saved before helper 0.8: no id to compare.
        let got = plan_candidates(&inputs(TransportKind::Tls), &Found { locator: keeper("k9", Some(40001)), ..host() }, "");
        assert!(!got.certain);
        assert_eq!(got.addrs.len(), 1);
    }

    #[test]
    fn open_ports_only_count_for_the_right_kind_of_phone() {
        let open = Found { direct_open: true, legacy_open: true, ..host() };
        assert!(plan_candidates(&inputs(TransportKind::Tls), &open, "").addrs.is_empty(), "a stranger's port 5555");

        let legacy = plan_candidates(&inputs(TransportKind::Legacy), &open, "");
        assert_eq!(legacy.addrs, vec![Candidate { addr: "192.168.1.84:5555".into(), transport: TransportKind::Legacy }]);

        let direct = plan_candidates(&inputs(TransportKind::Direct), &open, "");
        assert_eq!(direct.addrs, vec![Candidate { addr: "192.168.1.84:27190".into(), transport: TransportKind::Direct }]);
    }

    #[test]
    fn best_offer_jumps_the_queue_and_may_retry() {
        let mut t = Target {
            slot: 1,
            title: "P".into(),
            step: String::new(),
            queue: VecDeque::new(),
            tried: HashSet::from(["1.1.1.1:5".to_owned()]),
            last_try: Some(Instant::now()),
            found_ip: None,
            port_search_started: false,
            debugging_off: false,
            ok: false,
        };
        let c = |a: &str| Candidate { addr: a.into(), transport: TransportKind::Tls };
        t.offer(c("1.1.1.1:5"), false);
        assert!(t.queue.is_empty(), "already tried, no better news");
        t.offer(c("2.2.2.2:5"), false);
        t.offer(c("1.1.1.1:5"), true);
        assert_eq!(t.queue.front(), Some(&c("1.1.1.1:5")));
        assert_eq!(t.last_try, None);
    }

    #[test]
    fn failure_messages_say_what_to_do() {
        let mut t = Target {
            slot: 1,
            title: "Pixel".into(),
            step: String::new(),
            queue: VecDeque::new(),
            tried: HashSet::new(),
            last_try: None,
            found_ip: None,
            port_search_started: false,
            debugging_off: false,
            ok: false,
        };
        assert!(failure_message(&t).starts_with("Could not find Pixel"));
        t.found_ip = Some("192.168.1.84".parse().unwrap());
        assert!(failure_message(&t).contains("paired with another PC"));
        t.debugging_off = true;
        assert!(failure_message(&t).contains("Always allow"));
    }
}
