//! Avahi D-Bus client. Uses `Server2.ServiceBrowserPrepare` + `Start` so no
//! `ItemNew` signal can fire before we subscribe.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use zbus::Connection;
use zbus::zvariant::OwnedObjectPath;

use crate::{DiscoveredDevice, DiscoveryEvent, ServiceKind};

const IF_UNSPEC: i32 = -1;
/// `AVAHI_PROTO_INET`: browse and resolve over IPv4 only. Phones advertise on
/// both families; IPv4 avoids duplicate entries and link-local v6 scope ids.
const PROTO_INET: i32 = 0;
/// How long a rescan browser lives. Avahi queries at 1, 2, 4, 8, 16 s after
/// creating a browser, so this covers five fresh queries.
const RESCAN_FOR: Duration = Duration::from_secs(30);
/// Rescan requests closer together than this are merged.
const RESCAN_MIN_GAP: Duration = Duration::from_secs(5);

#[zbus::proxy(
    interface = "org.freedesktop.Avahi.Server2",
    default_service = "org.freedesktop.Avahi",
    default_path = "/"
)]
trait Server2 {
    fn get_version_string(&self) -> zbus::Result<String>;

    fn service_browser_prepare(
        &self,
        interface: i32,
        protocol: i32,
        service_type: &str,
        domain: &str,
        flags: u32,
    ) -> zbus::Result<OwnedObjectPath>;

    #[allow(clippy::too_many_arguments)]
    fn resolve_service(
        &self,
        interface: i32,
        protocol: i32,
        name: &str,
        service_type: &str,
        domain: &str,
        aprotocol: i32,
        flags: u32,
    ) -> zbus::Result<(i32, i32, String, String, String, String, i32, String, u16, Vec<Vec<u8>>, u32)>;
}

#[zbus::proxy(interface = "org.freedesktop.Avahi.ServiceBrowser", default_service = "org.freedesktop.Avahi")]
trait ServiceBrowser {
    fn start(&self) -> zbus::Result<()>;
    fn free(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn item_new(&self, interface: i32, protocol: i32, name: &str, service_type: &str, domain: &str, flags: u32)
    -> zbus::Result<()>;

    #[zbus(signal)]
    fn item_remove(
        &self,
        interface: i32,
        protocol: i32,
        name: &str,
        service_type: &str,
        domain: &str,
        flags: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn failure(&self, error: &str) -> zbus::Result<()>;
}

/// Starts browsing all ADB service types. Events are sent on `tx`; on
/// failure a single `DiscoveryEvent::Unavailable` is sent and the task ends.
///
/// Each `ServiceKind` sent on `rescan` starts a short-lived extra browser,
/// which makes Avahi query the network again right away. A long-running
/// browser backs off to one query an hour, and phones whose Wi-Fi was asleep
/// when it last asked are otherwise not seen until they re-announce.
pub fn spawn_avahi(tx: UnboundedSender<DiscoveryEvent>, rescan: UnboundedReceiver<ServiceKind>) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run(tx.clone(), rescan).await {
            tracing::warn!("avahi discovery unavailable: {e}");
            let _ = tx.send(DiscoveryEvent::Unavailable(e.to_string()));
        }
    })
}

async fn run(tx: UnboundedSender<DiscoveryEvent>, mut rescan: UnboundedReceiver<ServiceKind>) -> zbus::Result<()> {
    let conn = Connection::system().await?;
    let server = Server2Proxy::new(&conn).await?;
    tracing::info!("connected to {}", server.get_version_string().await?);

    let mut browsers = Vec::new();
    for kind in ServiceKind::ALL {
        let (browser, task) = start_browser(&conn, &server, kind, &tx).await?;
        browsers.push((browser, task));
    }
    let mut last: BTreeMap<ServiceKind, tokio::time::Instant> = BTreeMap::new();
    while let Some(kind) = rescan.recv().await {
        let now = tokio::time::Instant::now();
        if last.get(&kind).is_some_and(|t| now.duration_since(*t) < RESCAN_MIN_GAP) {
            continue;
        }
        last.insert(kind, now);
        tracing::debug!("rescanning {}", kind.service_type());
        match start_browser(&conn, &server, kind, &tx).await {
            Ok((browser, task)) => {
                tokio::spawn(async move {
                    tokio::time::sleep(RESCAN_FOR).await;
                    task.abort();
                    let _ = browser.free().await;
                });
            }
            Err(e) => tracing::warn!("rescan of {} failed: {e}", kind.service_type()),
        }
    }
    // The manager is gone.
    for (_, task) in browsers {
        task.abort();
    }
    Ok(())
}

async fn start_browser(
    conn: &Connection,
    server: &Server2Proxy<'static>,
    kind: ServiceKind,
    tx: &UnboundedSender<DiscoveryEvent>,
) -> zbus::Result<(ServiceBrowserProxy<'static>, JoinHandle<()>)> {
    let path = server.service_browser_prepare(IF_UNSPEC, PROTO_INET, kind.service_type(), "", 0).await?;
    let browser = ServiceBrowserProxy::builder(conn).path(path)?.build().await?;
    let new = browser.receive_item_new().await?;
    let removed = browser.receive_item_remove().await?;
    let failed = browser.receive_failure().await?;
    browser.start().await?;
    let task = tokio::spawn(browse(server.clone(), kind, new, removed, failed, tx.clone()));
    Ok((browser, task))
}

async fn browse(
    server: Server2Proxy<'static>,
    kind: ServiceKind,
    mut new: ItemNewStream,
    mut removed: ItemRemoveStream,
    mut failed: FailureStream,
    tx: UnboundedSender<DiscoveryEvent>,
) {
    loop {
        tokio::select! {
            Some(sig) = new.next() => {
                let Ok(a) = sig.args() else { continue };
                let (iface, name, domain) = (a.interface, a.name.to_owned(), a.domain.to_owned());
                // Resolve off the signal loop; a slow phone must not stall others.
                let (server, tx) = (server.clone(), tx.clone());
                tokio::spawn(async move {
                    match resolve(&server, iface, &name, kind, &domain).await {
                        Ok(dev) => { let _ = tx.send(DiscoveryEvent::Added(dev)); }
                        Err(e) => tracing::debug!("resolve {name} ({}) failed: {e}", kind.service_type()),
                    }
                });
            }
            Some(sig) = removed.next() => {
                if let Ok(a) = sig.args() {
                    let _ = tx.send(DiscoveryEvent::Removed { instance: a.name.to_owned(), kind });
                }
            }
            Some(sig) = failed.next() => {
                if let Ok(a) = sig.args() {
                    tracing::warn!("avahi browser {} failed: {}", kind.service_type(), a.error);
                }
            }
            else => return,
        }
        if tx.is_closed() {
            return;
        }
    }
}

async fn resolve(
    server: &Server2Proxy<'_>,
    iface: i32,
    name: &str,
    kind: ServiceKind,
    domain: &str,
) -> zbus::Result<DiscoveredDevice> {
    let (_, _, name, _, _, host, _, address, port, txt, _) =
        server.resolve_service(iface, PROTO_INET, name, kind.service_type(), domain, PROTO_INET, 0).await?;
    let ip: IpAddr = address
        .parse()
        .map_err(|_| zbus::Error::Failure(format!("bad address {address:?}")))?;
    Ok(DiscoveredDevice { instance: name, kind, addr: SocketAddr::new(ip, port), host, txt: parse_txt(&txt) })
}

fn parse_txt(records: &[Vec<u8>]) -> BTreeMap<String, String> {
    records
        .iter()
        .filter_map(|r| {
            let s = String::from_utf8_lossy(r);
            let (k, v) = s.split_once('=').unwrap_or((&s, ""));
            (!k.is_empty()).then(|| (k.to_owned(), v.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_txt_records() {
        let txt = parse_txt(&[b"v=ADB_SECURE_SERVICE_VERSION".to_vec(), b"flag".to_vec(), b"=x".to_vec()]);
        assert_eq!(txt.get("v").map(String::as_str), Some("ADB_SECURE_SERVICE_VERSION"));
        assert_eq!(txt.get("flag").map(String::as_str), Some(""));
        assert_eq!(txt.len(), 2);
    }
}
