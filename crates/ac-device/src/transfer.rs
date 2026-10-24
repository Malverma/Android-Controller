//! File transfer jobs (spec §5.3–§5.9): segmented, resumable pushes over
//! `sync:` and streamed APK installs, with per-slot progress for the UI.
//!
//! Jobs live in memory: they survive Wi-Fi drops and reconnects (resuming at
//! the last committed segment) but not an app restart.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use ac_adb::sync::MODE_REGULAR_644;
use ac_adb::{AdbClient, AdbError, quote};
use ac_config::Transfer;
use ac_core::SlotId;
use ac_scrcpy::ControlMsg;
use ac_transfer::{Item, TokenBucket, apk, free_name, is_media, join, part_size_resume, segments, url_encode_path};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Semaphore, watch};
use tokio::task::AbortHandle;

use crate::api::{TransferView, UiEvent};
use crate::stream::SlotIo;

/// Host reads (spec §5.4).
const READ_BLOCK: usize = 1 << 20;
/// A write that makes no progress this long means the link is dead.
const STALL: Duration = Duration::from_secs(30);
/// Transient failures tolerated while the phone stays connected.
const MAX_RETRIES: u32 = 5;
/// Checksum mismatches before giving up (spec §5.5 step 4).
const MAX_CHECKSUM_RETRIES: u32 = 2;
/// Free space kept on the phone beyond the file itself.
const SPACE_MARGIN: u64 = 64 << 20;
const REPORT_EVERY: Duration = Duration::from_millis(250);
/// Scrcpy's `SCAN_FILE` path limit.
const SCAN_FILE_PATH_MAX: usize = 256;

/// The phone a slot currently reaches (set by the manager while Online).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub serial: String,
    pub device_id: String,
    pub shell_v2: bool,
    pub sdk: u32,
    /// Set for a phone reached without ADB: files go through the helper.
    pub direct: Option<DirectTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectTarget {
    pub addr: std::net::SocketAddr,
    pub creds: ac_direct::Credentials,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Push,
    Install,
}

struct Job {
    id: u64,
    kind: Kind,
    device_id: String,
    local: PathBuf,
    name: String,
    remote_dir: String,
    size: u64,
    mtime: u32,
    /// Decided once (conflict handling), kept across retries.
    final_path: Option<String>,
    part: String,
    seg: String,
    committed: u64,
    /// SHA-256 state of bytes `[0, committed)`, when verifying.
    hash: Option<Sha256>,
}

#[derive(Debug)]
enum JobError {
    /// Worth retrying once the phone is reachable.
    Retry(String),
    Fatal(String),
}

impl From<AdbError> for JobError {
    fn from(e: AdbError) -> Self {
        classify(&e.to_string())
    }
}

impl From<std::io::Error> for JobError {
    fn from(e: std::io::Error) -> Self {
        JobError::Retry(e.to_string())
    }
}

/// Device-side failures that retrying cannot fix (spec §5.6).
fn classify(msg: &str) -> JobError {
    let m = msg.to_ascii_lowercase();
    if m.contains("no space left") {
        JobError::Fatal("Not enough free space on the phone".into())
    } else if m.contains("permission denied") || m.contains("read-only file system") {
        JobError::Fatal(format!("The phone refused the write: {}", msg.trim()))
    } else {
        JobError::Retry(msg.trim().to_owned())
    }
}

enum Outcome {
    Sent,
    Installed,
    Skipped,
    Failed(String),
}

/// Aggregate of one slot's transfers, from the first queued job until all finish.
#[derive(Default)]
struct Batch {
    count: u32,
    finished: u32,
    total: u64,
    finished_bytes: u64,
    progress: HashMap<u64, u64>,
    current: Option<(u64, String, Kind)>,
    sent_files: u32,
    skipped: u32,
    installed: Vec<String>,
    failed: Vec<(String, String)>,
    /// Remote scratch files to delete if the job is cancelled.
    cleanup: HashMap<u64, (Link, Vec<String>)>,
    aborts: HashMap<u64, AbortHandle>,
    waiting: bool,
    started: Option<Instant>,
    title: String,
    names: HashMap<u64, String>,
    /// Final paths reserved by running jobs, released on cancel.
    finals: HashMap<u64, (String, String)>,
}

impl Batch {
    fn sent(&self) -> u64 {
        self.finished_bytes + self.progress.values().sum::<u64>()
    }
}

struct SlotShared {
    slot: SlotId,
    batch: Mutex<Batch>,
    link: watch::Receiver<Option<Link>>,
    /// One job per device at a time beyond this (spec §5.3).
    sem: Semaphore,
}

struct Ctx {
    cfg: Transfer,
    adb: AdbClient,
    io: Vec<Arc<SlotIo>>,
    slots: Vec<SlotShared>,
    global: Semaphore,
    /// `device_id` + remote path of every final name in flight.
    reserved: Mutex<HashSet<(String, String)>>,
    next_id: AtomicU64,
    ui: async_channel::Sender<UiEvent>,
}

pub struct Transfers {
    ctx: Arc<Ctx>,
    links: Vec<watch::Sender<Option<Link>>>,
}

impl Transfers {
    pub fn new(cfg: Transfer, adb_server: std::net::SocketAddr, io: Vec<Arc<SlotIo>>, ui: async_channel::Sender<UiEvent>) -> Self {
        let (links, rxs): (Vec<_>, Vec<_>) = io.iter().map(|_| watch::channel(None)).unzip();
        let per_device = cfg.max_concurrent_per_device.clamp(1, 8) as usize;
        let slots = rxs
            .into_iter()
            .enumerate()
            .map(|(i, link)| SlotShared {
                slot: i as SlotId + 1,
                batch: Mutex::default(),
                link,
                sem: Semaphore::new(per_device),
            })
            .collect();
        let ctx = Arc::new(Ctx {
            global: Semaphore::new(cfg.max_concurrent_global.clamp(1, 16) as usize),
            cfg,
            // Device-side checksums and appends of big segments take a while.
            adb: AdbClient::new(adb_server).with_timeout(Duration::from_secs(120)),
            io,
            slots,
            reserved: Mutex::default(),
            next_id: AtomicU64::new(rand::random::<u32>() as u64),
            ui,
        });
        tokio::spawn(report(Arc::downgrade(&ctx)));
        Self { ctx, links }
    }

    /// Called by the manager whenever a slot's phone comes or goes.
    pub fn set_link(&self, slot: SlotId, link: Option<Link>) {
        if let Some(tx) = self.links.get((slot as usize).wrapping_sub(1)) {
            tx.send_if_modified(|cur| {
                let changed = *cur != link;
                *cur = link;
                changed
            });
        }
    }

    pub fn is_active(&self, slot: SlotId) -> bool {
        self.ctx.slot(slot).is_some_and(|s| s.batch.lock().unwrap().count > 0)
    }

    /// Queues `paths` (files and folders) for the phone `device_id` in `slot`.
    /// APKs are installed when `install_apks`, otherwise pushed like any file.
    pub fn submit(&self, slot: SlotId, device_id: String, title: String, paths: Vec<PathBuf>, install_apks: bool) {
        let ctx = self.ctx.clone();
        tokio::spawn(async move {
            let cfg = ctx.cfg.clone();
            let expanded = tokio::task::spawn_blocking(move || {
                let (items, errors) = ac_transfer::expand(&paths, cfg.recursive_directories);
                let items: Vec<(Item, bool)> = items
                    .into_iter()
                    .map(|it| {
                        let install = install_apks && it.rel_dir.is_empty() && apk::is_apk(&it.local);
                        (it, install)
                    })
                    .collect();
                (items, errors)
            })
            .await;
            let Ok((items, errors)) = expanded else { return };
            for e in &errors {
                ctx.toast(format!("Can't read {}", e.display()));
            }
            if items.is_empty() {
                return;
            }
            ctx.enqueue(slot, device_id, title, items);
        });
    }

    /// Cancels every queued and running transfer for `slot`.
    pub fn cancel(&self, slot: SlotId) {
        let Some(s) = self.ctx.slot(slot) else { return };
        let (aborts, cleanup, n) = {
            let mut b = s.batch.lock().unwrap();
            if b.count == 0 {
                return;
            }
            let aborts: Vec<_> = b.aborts.drain().map(|(_, h)| h).collect();
            let cleanup: Vec<_> = b.cleanup.drain().map(|(_, c)| c).collect();
            let mut reserved = self.ctx.reserved.lock().unwrap();
            for (_, key) in b.finals.drain() {
                reserved.remove(&key);
            }
            let n = b.count - b.finished;
            *b = Batch::default();
            (aborts, cleanup, n)
        };
        for h in aborts {
            h.abort();
        }
        let adb = self.ctx.adb.clone();
        tokio::spawn(async move {
            // Let an append the phone may still be running finish first.
            tokio::time::sleep(Duration::from_millis(500)).await;
            for (link, files) in cleanup {
                let args: Vec<_> = files.iter().filter_map(|f| quote(f).ok()).collect();
                let _ = adb.shell(&link.serial, &format!("rm -f {}", args.join(" ")), link.shell_v2).await;
            }
        });
        self.ctx.send_view(s);
        self.ctx.toast(format!("Cancelled {n} transfer{}", if n == 1 { "" } else { "s" }));
    }
}

impl Ctx {
    fn slot(&self, slot: SlotId) -> Option<&SlotShared> {
        self.slots.get((slot as usize).wrapping_sub(1))
    }

    fn toast(&self, message: String) {
        let _ = self.ui.try_send(UiEvent::Toast { code: None, message });
    }

    fn enqueue(self: &Arc<Self>, slot: SlotId, device_id: String, title: String, items: Vec<(Item, bool)>) {
        let Some(s) = self.slot(slot) else { return };
        let mut b = s.batch.lock().unwrap();
        if b.count == 0 {
            *b = Batch { started: Some(Instant::now()), ..Batch::default() };
        }
        b.title = title;
        for (it, install) in items {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let name = it.local.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let remote_dir = join(&self.cfg.default_target_dir, &[&it.rel_dir]);
            let mtime = std::fs::metadata(&it.local)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as u32);
            let verify = match self.cfg.verify_checksum.as_str() {
                "always" => true,
                "never" => false,
                _ => it.size >= self.cfg.verify_checksum_threshold_mib as u64 * (1 << 20),
            };
            let job = Job {
                id,
                kind: if install { Kind::Install } else { Kind::Push },
                device_id: device_id.clone(),
                part: join(&remote_dir, &[&format!(".ac-{id:016x}.part")]),
                seg: join(&remote_dir, &[&format!(".ac-{id:016x}.seg")]),
                local: it.local,
                name,
                remote_dir,
                size: it.size,
                mtime,
                final_path: None,
                committed: 0,
                hash: (verify && !install).then(Sha256::new),
            };
            b.names.insert(id, job.name.clone());
            b.count += 1;
            b.total += it.size;
            let ctx = self.clone();
            let h = tokio::spawn(async move {
                let size = job.size;
                let outcome = ctx.run(slot, job, id).await;
                ctx.finish(slot, id, size, outcome);
            });
            b.aborts.insert(id, h.abort_handle());
        }
    }

    fn segment_size(&self) -> u64 {
        self.cfg.segment_size_mib.clamp(4, 256) as u64 * (1 << 20)
    }

    fn finish(&self, slot: SlotId, id: u64, size: u64, outcome: Outcome) {
        let Some(s) = self.slot(slot) else { return };
        let mut b = s.batch.lock().unwrap();
        if b.aborts.remove(&id).is_none() {
            return; // cancelled meanwhile
        }
        b.cleanup.remove(&id);
        b.finals.remove(&id);
        b.progress.remove(&id);
        let name = b.names.remove(&id).unwrap_or_default();
        b.finished += 1;
        b.finished_bytes += size;
        if b.current.as_ref().is_some_and(|c| c.0 == id) {
            b.current = None;
        }
        match outcome {
            Outcome::Sent => b.sent_files += 1,
            Outcome::Installed => b.installed.push(name),
            Outcome::Skipped => b.skipped += 1,
            Outcome::Failed(why) => {
                tracing::warn!(slot, file = %name, "transfer failed: {why}");
                b.failed.push((name, why));
            }
        }
        if b.finished < b.count {
            return;
        }
        let done = std::mem::take(&mut *b);
        drop(b);
        self.send_view(s);
        self.toast(summary(&done));
    }

    fn send_view(&self, s: &SlotShared) {
        let b = s.batch.lock().unwrap();
        let _ = self.ui.try_send(UiEvent::Transfer(view(s.slot, &b, 0)));
    }

    /// Waits until `slot` reaches the phone `device_id`.
    async fn link(&self, slot: SlotId, id: u64, device_id: &str) -> Link {
        let s = self.slot(slot).expect("valid slot");
        let mut rx = s.link.clone();
        loop {
            let cur = rx.borrow_and_update().clone();
            if let Some(l) = cur.filter(|l| l.device_id == device_id) {
                let mut b = s.batch.lock().unwrap();
                if b.current.as_ref().is_none_or(|c| c.0 == id) {
                    b.waiting = false;
                }
                return l;
            }
            s.batch.lock().unwrap().waiting = true;
            if rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    fn still_linked(&self, slot: SlotId, link: &Link) -> bool {
        self.slot(slot).is_some_and(|s| s.link.borrow().as_ref() == Some(link))
    }

    fn set_progress(&self, slot: SlotId, id: u64, bytes: u64) {
        if let Some(s) = self.slot(slot) {
            let mut b = s.batch.lock().unwrap();
            if b.aborts.contains_key(&id) {
                b.progress.insert(id, bytes);
            }
        }
    }

    async fn run(&self, slot: SlotId, mut job: Job, id: u64) -> Outcome {
        let s = self.slot(slot).expect("valid slot");
        let Ok(_dev) = s.sem.acquire().await else { return Outcome::Failed("shut down".into()) };
        let Ok(_all) = self.global.acquire().await else { return Outcome::Failed("shut down".into()) };
        s.batch.lock().unwrap().current = Some((id, job.name.clone(), job.kind));
        let mut retries = 0;
        let mut checksum_retries = 0;
        loop {
            let link = self.link(slot, id, &job.device_id).await;
            let r = match (job.kind, &link.direct) {
                (Kind::Push, None) => self.push(slot, &link, &mut job).await,
                (Kind::Install, None) => self.install(slot, &link, &job).await,
                (kind, Some(d)) => self.send_direct(slot, &link, d, &job, kind == Kind::Install).await,
            };
            match r {
                Ok(o) => {
                    if let Some(f) = &job.final_path {
                        self.reserved.lock().unwrap().remove(&(job.device_id.clone(), f.clone()));
                    }
                    return o;
                }
                Err(JobError::Fatal(why)) if why == CHECKSUM && checksum_retries < MAX_CHECKSUM_RETRIES => {
                    checksum_retries += 1;
                    job.committed = 0;
                    job.hash = Some(Sha256::new());
                }
                Err(JobError::Fatal(why)) => {
                    if link.direct.is_none() {
                        let _ = self.rm(&link, &[&job.part, &job.seg]).await;
                    }
                    if let Some(f) = &job.final_path {
                        self.reserved.lock().unwrap().remove(&(job.device_id.clone(), f.clone()));
                    }
                    return Outcome::Failed(why);
                }
                Err(JobError::Retry(why)) => {
                    tracing::info!(slot, file = %job.name, "transfer interrupted: {why}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    // Drops caused by the phone going away don't count.
                    if self.still_linked(slot, &link) {
                        retries += 1;
                        if retries > MAX_RETRIES {
                            if link.direct.is_none() {
                                let _ = self.rm(&link, &[&job.part, &job.seg]).await;
                            }
                            return Outcome::Failed(why);
                        }
                    }
                }
            }
        }
    }

    async fn sh(&self, link: &Link, cmd: &str) -> Result<String, JobError> {
        let out = self.adb.shell(&link.serial, cmd, link.shell_v2).await?;
        Ok(format!("{}{}", out.stdout, out.stderr))
    }

    /// Runs `cmd` and requires it to succeed (it must end in `&& echo @ok`).
    async fn sh_ok(&self, link: &Link, cmd: &str) -> Result<String, JobError> {
        let out = self.sh(link, cmd).await?;
        if out.lines().any(|l| l.trim() == "@ok") {
            Ok(out)
        } else {
            Err(classify(&out))
        }
    }

    async fn rm(&self, link: &Link, files: &[&str]) -> Result<(), JobError> {
        let args: Result<Vec<_>, _> = files.iter().map(|f| quote(f)).collect();
        self.sh(link, &format!("rm -f {}", args.map_err(|e| JobError::Fatal(e.to_string()))?.join(" "))).await?;
        Ok(())
    }

    async fn push(&self, slot: SlotId, link: &Link, job: &mut Job) -> Result<Outcome, JobError> {
        let q = |p: &str| quote(p).map_err(|e| JobError::Fatal(e.to_string()));
        let segment = self.segment_size();
        let (dir, part, seg) = (q(&job.remote_dir)?, q(&job.part)?, q(&job.seg)?);
        if let Some(s) = self.slot(slot) {
            s.batch.lock().unwrap().cleanup.insert(job.id, (link.clone(), vec![job.part.clone(), job.seg.clone()]));
        }

        // Preflight (spec §5.5 step 1).
        let final_probe = q(job.final_path.as_deref().unwrap_or(&join(&job.remote_dir, &[&job.name])))?;
        let out = self
            .sh_ok(
                link,
                &format!(
                    "mkdir -p {dir} && echo @ok; df -k {dir} 2>/dev/null | tail -1; \
                     [ -e {final_probe} ] && echo @E; [ -e {part} ] && echo \"@P $(stat -c %s {part})\"; rm -f {seg}"
                ),
            )
            .await?;
        let exists = out.lines().any(|l| l.trim() == "@E");
        let part_size = out.lines().find_map(|l| l.trim().strip_prefix("@P ")?.trim().parse::<u64>().ok());
        if job.final_path.is_none() {
            match self.resolve_final(link, job, exists).await? {
                Some(f) => job.final_path = Some(f),
                None => return Ok(Outcome::Skipped),
            }
        }
        let final_path = job.final_path.clone().expect("resolved");
        if let Some(s) = self.slot(slot) {
            s.batch.lock().unwrap().finals.insert(job.id, (job.device_id.clone(), final_path.clone()));
        }
        let fin = q(&final_path)?;

        let resume = part_size.map_or(0, |n| part_size_resume(n, segment, job.size));
        // The device and our hash state must agree on the prefix.
        if job.hash.is_some() && (resume != job.committed || resume == 0) {
            job.hash = Some(if resume == 0 { Sha256::new() } else { hash_prefix(&job.local, resume).await? });
        }
        if resume != job.committed || part_size.is_some_and(|n| n != resume) {
            job.committed = resume;
            if resume == 0 {
                self.sh(link, &format!("rm -f {part}")).await?;
            } else if part_size != Some(resume) {
                self.sh_ok(link, &format!("truncate -s {resume} {part} && echo @ok")).await?;
            }
        }
        if let Some(avail) = parse_df(&out)
            && avail < job.size - job.committed + SPACE_MARGIN
        {
            return Err(JobError::Fatal(format!(
                "Not enough free space on the phone ({} free, {} needed)",
                human_bytes(avail),
                human_bytes(job.size - job.committed)
            )));
        }

        // Uploading (step 2).
        self.set_progress(slot, job.id, job.committed);
        let mut file = tokio::fs::File::open(&job.local).await.map_err(|e| JobError::Fatal(format!("Can't read {}: {e}", job.local.display())))?;
        let mut bucket = TokenBucket::new(self.rate(slot), Instant::now());
        let mut buf = vec![0u8; READ_BLOCK];
        for (start, len) in segments(job.size, job.committed, segment) {
            self.check_local(job).await?;
            let direct = start == 0; // first segment goes straight to the part
            let target = if direct { &job.part } else { &job.seg };
            let mut hasher = job.hash.clone();
            let mut sync = self.adb.sync(&link.serial).await?;
            sync.send_start(target, MODE_REGULAR_644).await?;
            file.seek(std::io::SeekFrom::Start(start)).await?;
            let mut sent = 0u64;
            while sent < len {
                let n = (len - sent).min(READ_BLOCK as u64) as usize;
                file.read_exact(&mut buf[..n]).await.map_err(|e| JobError::Fatal(format!("Can't read {}: {e}", job.local.display())))?;
                bucket.set_rate(self.rate(slot), Instant::now());
                let wait = bucket.take(n as u64, Instant::now());
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
                tokio::time::timeout(STALL, sync.send_data(&buf[..n])).await.map_err(|_| JobError::Retry("stalled".into()))??;
                if let Some(h) = &mut hasher {
                    h.update(&buf[..n]);
                }
                sent += n as u64;
                self.set_progress(slot, job.id, start + sent);
            }
            tokio::time::timeout(STALL, sync.send_done(job.mtime)).await.map_err(|_| JobError::Retry("stalled".into()))??;
            let _ = sync.quit().await;
            if !direct {
                let out = self.sh_ok(link, &format!("cat {seg} >> {part} && rm -f {seg} && stat -c %s {part} && echo @ok")).await?;
                let got = out.lines().find_map(|l| l.trim().parse::<u64>().ok());
                if got != Some(start + len) {
                    return Err(JobError::Retry(format!("part is {got:?} bytes, expected {}", start + len)));
                }
            }
            job.committed = start + len;
            job.hash = hasher;
        }

        // Finalizing and verifying (steps 3–4).
        let digest = job.hash.clone().map(|h| hex(&h.finalize()));
        let sum = if digest.is_some() { format!(" && sha256sum {fin}") } else { String::new() };
        let out = self.sh_ok(link, &format!("mv -f {part} {fin} && stat -c %s {fin}{sum} && echo @ok")).await?;
        let mut lines = out.lines().map(str::trim);
        let remote_size = lines.find_map(|l| l.parse::<u64>().ok());
        if remote_size != Some(job.size) {
            let _ = self.rm(link, &[&final_path]).await;
            job.committed = 0;
            return Err(JobError::Retry(format!("phone has {remote_size:?} bytes, expected {}", job.size)));
        }
        if let Some(want) = digest {
            let got = out.lines().find_map(|l| l.split_whitespace().next().filter(|w| w.len() == 64));
            if got != Some(want.as_str()) {
                let _ = self.rm(link, &[&final_path]).await;
                return Err(JobError::Fatal(CHECKSUM.into()));
            }
        }
        // Scanning (step 5).
        if self.cfg.trigger_media_scan && is_media(&job.name) {
            self.scan(slot, link, &final_path).await;
        }
        Ok(Outcome::Sent)
    }

    /// Chooses the final remote path per `on_conflict`; `None` = skip.
    async fn resolve_final(&self, link: &Link, job: &Job, exists: bool) -> Result<Option<String>, JobError> {
        let want = join(&job.remote_dir, &[&job.name]);
        let key = |p: &str| (job.device_id.clone(), p.to_owned());
        let in_flight = self.reserved.lock().unwrap().contains(&key(&want));
        let chosen = match self.cfg.on_conflict.as_str() {
            "overwrite" => want,
            "skip" if exists || in_flight => return Ok(None),
            _ if !exists && !in_flight => want,
            "skip" => unreachable!(),
            _ => {
                let mut taken: HashSet<String> = if exists {
                    let dir = quote(&job.remote_dir).map_err(|e| JobError::Fatal(e.to_string()))?;
                    self.sh(link, &format!("ls -1a {dir}")).await?.lines().map(str::to_owned).collect()
                } else {
                    HashSet::new()
                };
                let prefix = format!("{}/", job.remote_dir.trim_end_matches('/'));
                for (dev, p) in self.reserved.lock().unwrap().iter() {
                    if *dev == job.device_id
                        && let Some(n) = p.strip_prefix(&prefix)
                    {
                        taken.insert(n.to_owned());
                    }
                }
                join(&job.remote_dir, &[&free_name(&job.name, &taken)])
            }
        };
        self.reserved.lock().unwrap().insert(key(&chosen));
        Ok(Some(chosen))
    }

    /// The local file must not change under us (spec §5.6).
    async fn check_local(&self, job: &Job) -> Result<(), JobError> {
        let m = tokio::fs::metadata(&job.local).await.map_err(|e| JobError::Fatal(format!("{}: {e}", job.local.display())))?;
        let mtime = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs() as u32);
        if m.len() != job.size || mtime != job.mtime {
            return Err(JobError::Fatal(format!("{} changed while it was being sent", job.name)));
        }
        Ok(())
    }

    fn rate(&self, slot: SlotId) -> u64 {
        let streaming = self.io.get(slot as usize - 1).is_some_and(|io| io.is_streaming());
        let mbps = if streaming { self.cfg.rate_limit_while_streaming_mbps } else { self.cfg.rate_limit_idle_mbps };
        mbps as u64 * 1_000_000 / 8
    }

    async fn scan(&self, slot: SlotId, link: &Link, path: &str) {
        let io = &self.io[slot as usize - 1];
        if path.len() <= SCAN_FILE_PATH_MAX && io.send(ControlMsg::ScanFile(path.to_owned())) {
            return;
        }
        let uri = format!("file://{}", url_encode_path(path));
        if let Ok(uri) = quote(&uri) {
            let _ = self
                .sh(link, &format!("am broadcast -a android.intent.action.MEDIA_SCANNER_SCAN_FILE -d {uri} >/dev/null 2>&1"))
                .await;
        }
    }

    /// Streams an APK into the package manager (spec §5.9).
    async fn install(&self, slot: SlotId, link: &Link, job: &Job) -> Result<Outcome, JobError> {
        if link.sdk != 0 && link.sdk < 24 {
            return Err(JobError::Fatal("Installing needs Android 7 or newer; drop with Shift to copy the APK instead".into()));
        }
        self.check_local(job).await?;
        self.set_progress(slot, job.id, 0);
        let mut file = tokio::fs::File::open(&job.local).await.map_err(|e| JobError::Fatal(format!("Can't read {}: {e}", job.local.display())))?;
        let mut s = self.adb.open_service(&link.serial, &format!("exec:cmd package install -r -S {}", job.size)).await?;
        let mut bucket = TokenBucket::new(self.rate(slot), Instant::now());
        let mut buf = vec![0u8; READ_BLOCK];
        let mut sent = 0u64;
        while sent < job.size {
            let n = (job.size - sent).min(READ_BLOCK as u64) as usize;
            file.read_exact(&mut buf[..n]).await.map_err(|e| JobError::Fatal(format!("Can't read {}: {e}", job.local.display())))?;
            let wait = bucket.take(n as u64, Instant::now());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            tokio::time::timeout(STALL, s.write_all(&buf[..n])).await.map_err(|_| JobError::Retry("stalled".into()))??;
            sent += n as u64;
            self.set_progress(slot, job.id, sent);
        }
        s.flush().await?;
        let mut out = String::new();
        tokio::time::timeout(Duration::from_secs(180), s.read_to_string(&mut out))
            .await
            .map_err(|_| JobError::Retry("the phone did not answer the install".into()))??;
        match parse_install(&out) {
            Ok(()) => Ok(Outcome::Installed),
            Err(why) => Err(JobError::Fatal(why)),
        }
    }
}

impl Ctx {
    /// A file or APK through the Android helper (no ADB): one resumable
    /// upload; the phone picks the final name and reports its checksum.
    async fn send_direct(&self, slot: SlotId, link: &Link, d: &DirectTarget, job: &Job, install: bool) -> Result<Outcome, JobError> {
        self.check_local(job).await?;
        let base = self.cfg.default_target_dir.trim_end_matches('/');
        let dir = job.remote_dir.strip_prefix(base).unwrap_or("").trim_matches('/').to_owned();
        let req = ac_direct::PushRequest {
            name: job.name.clone(),
            dir,
            size: job.size,
            job: job.id,
            install,
            conflict: self.cfg.on_conflict.clone(),
        };
        let (mut s, start) = ac_direct::open_push(d.addr, &d.creds, &link.device_id, &req).await.map_err(direct_err)?;
        let offset = match start {
            ac_direct::PushStart::Skip => return Ok(Outcome::Skipped),
            ac_direct::PushStart::From(o) => o,
        };
        self.set_progress(slot, job.id, offset);
        let mut file = tokio::fs::File::open(&job.local).await.map_err(|e| JobError::Fatal(format!("Can't read {}: {e}", job.local.display())))?;
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut bucket = TokenBucket::new(self.rate(slot), Instant::now());
        let mut buf = vec![0u8; READ_BLOCK];
        let mut sent = offset;
        while sent < job.size {
            let n = (job.size - sent).min(READ_BLOCK as u64) as usize;
            file.read_exact(&mut buf[..n]).await.map_err(|e| JobError::Fatal(format!("Can't read {}: {e}", job.local.display())))?;
            bucket.set_rate(self.rate(slot), Instant::now());
            let wait = bucket.take(n as u64, Instant::now());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            tokio::time::timeout(STALL, s.write_all(&buf[..n])).await.map_err(|_| JobError::Retry("stalled".into()))??;
            sent += n as u64;
            self.set_progress(slot, job.id, sent);
        }
        s.flush().await?;
        // An install waits for the user to confirm on the phone.
        let limit = if install { Duration::from_secs(6 * 60) } else { Duration::from_secs(120) };
        let done = ac_direct::finish_push(&mut s, limit).await.map_err(direct_err)?;
        if install {
            return if done.installed { Ok(Outcome::Installed) } else { Err(JobError::Fatal("the phone did not install it".into())) };
        }
        if job.hash.is_some() {
            let want = hex(&hash_prefix(&job.local, job.size).await?.finalize());
            if done.sha256.as_deref() != Some(want.as_str()) {
                return Err(JobError::Fatal(format!("{CHECKSUM} (the phone kept {})", done.path.unwrap_or_default())));
            }
        }
        Ok(Outcome::Sent)
    }
}

fn direct_err(e: ac_direct::Error) -> JobError {
    match e {
        ac_direct::Error::Refused { message, .. } => JobError::Fatal(message),
        other => JobError::Retry(other.to_string()),
    }
}

const CHECKSUM: &str = "Checksum mismatch after sending";

async fn hash_prefix(path: &Path, len: u64) -> Result<Sha256, JobError> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let f = std::fs::File::open(&path)?;
        let mut h = Sha256::new();
        let mut r = f.take(len);
        let mut buf = vec![0u8; READ_BLOCK];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok::<_, std::io::Error>(h)
    })
    .await
    .map_err(|e| JobError::Fatal(e.to_string()))?
    .map_err(|e| JobError::Fatal(e.to_string()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Available bytes from the last line of `df -k <dir>` (4th column).
fn parse_df(out: &str) -> Option<u64> {
    out.lines()
        .filter(|l| !l.starts_with('@'))
        .find_map(|l| {
            let f: Vec<_> = l.split_whitespace().collect();
            (f.len() >= 4).then(|| f[3].parse::<u64>().ok()).flatten()
        })
        .map(|k| k * 1024)
}

/// `cmd package install` output → Ok, or the reason with a hint.
fn parse_install(out: &str) -> Result<(), String> {
    if out.lines().any(|l| l.trim() == "Success") {
        return Ok(());
    }
    let reason = out
        .split_once("Failure [")
        .and_then(|(_, r)| r.split_once(']'))
        .map(|(r, _)| r.trim().to_owned())
        .unwrap_or_else(|| out.trim().lines().last().unwrap_or("no answer from the phone").to_owned());
    let hint = match reason.split(':').next().unwrap_or_default().trim() {
        "INSTALL_FAILED_USER_RESTRICTED" => " — enable \"Install via USB\" in Developer options",
        "INSTALL_FAILED_UPDATE_INCOMPATIBLE" => " — signature differs; uninstall the existing app first",
        "INSTALL_FAILED_VERSION_DOWNGRADE" => " — the phone has a newer version",
        "INSTALL_FAILED_INSUFFICIENT_STORAGE" => " — not enough free space",
        "INSTALL_FAILED_OLDER_SDK" => " — the app needs a newer Android version",
        "INSTALL_FAILED_NO_MATCHING_ABIS" => " — the app is not built for this phone's CPU",
        "INSTALL_PARSE_FAILED_NO_CERTIFICATES" => " — the APK is not signed",
        _ => "",
    };
    Err(format!("Install failed: {reason}{hint}"))
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

fn view(slot: SlotId, b: &Batch, rate_bps: u64) -> TransferView {
    let sent = b.sent();
    let elapsed = b.started.map_or(Duration::ZERO, |t| t.elapsed());
    TransferView {
        slot,
        active: b.count > 0,
        file: b.current.as_ref().map(|c| c.1.clone()).unwrap_or_default(),
        installing: b.current.as_ref().is_some_and(|c| c.2 == Kind::Install),
        index: (b.finished + 1).min(b.count),
        count: b.count,
        sent,
        total: b.total,
        rate_bps,
        eta_s: (elapsed >= Duration::from_secs(2) && rate_bps > 0).then(|| b.total.saturating_sub(sent) / rate_bps),
        waiting: b.waiting,
    }
}

fn summary(b: &Batch) -> String {
    let plural = |n: u32| if n == 1 { "" } else { "s" };
    let mut parts = Vec::new();
    if b.sent_files > 0 {
        parts.push(format!("{} file{} sent to {}", b.sent_files, plural(b.sent_files), b.title));
    }
    match b.installed.as_slice() {
        [] => {}
        [one] => parts.push(format!("Installed {one} on {}", b.title)),
        many => parts.push(format!("Installed {} apps on {}", many.len(), b.title)),
    }
    if b.skipped > 0 {
        parts.push(format!("{} skipped (already on the phone)", b.skipped));
    }
    match b.failed.as_slice() {
        [] => {}
        [(name, why)] if name.is_empty() => parts.push(why.clone()),
        [(name, why)] => parts.push(format!("{name}: {why}")),
        [.., (_, why)] => parts.push(format!("{} failed ({why})", b.failed.len())),
    }
    parts.join(" · ")
}

/// Sends each active slot's progress at 4 Hz with a smoothed rate (spec §5.7).
async fn report(ctx: std::sync::Weak<Ctx>) {
    let mut t = tokio::time::interval(REPORT_EVERY);
    let mut last: HashMap<SlotId, (u64, f64)> = HashMap::new();
    loop {
        t.tick().await;
        let Some(ctx) = ctx.upgrade() else { return };
        for s in &ctx.slots {
            let b = s.batch.lock().unwrap();
            if b.count == 0 {
                last.remove(&s.slot);
                continue;
            }
            let sent = b.sent();
            let (prev, ewma) = last.get(&s.slot).copied().unwrap_or((sent, 0.0));
            let inst = sent.saturating_sub(prev) as f64 / REPORT_EVERY.as_secs_f64();
            let ewma = if ewma == 0.0 { inst } else { 0.3 * inst + 0.7 * ewma };
            last.insert(s.slot, (sent, ewma));
            let _ = ctx.ui.try_send(UiEvent::Transfer(view(s.slot, &b, ewma as u64)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_df() {
        let out = "@ok\n/dev/fuse        115276880 52441164  62704644  46% /storage/emulated\n@P 5\n";
        assert_eq!(parse_df(out), Some(62_704_644 * 1024));
        assert_eq!(parse_df("@ok\n"), None);
    }

    #[test]
    fn install_results() {
        assert_eq!(parse_install("Performing Streamed Install\nSuccess\n"), Ok(()));
        let e = parse_install("Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Package x signatures do not match]").unwrap_err();
        assert!(e.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") && e.contains("uninstall"), "{e}");
        assert!(parse_install("").unwrap_err().contains("no answer"));
    }

    #[test]
    fn classifies_device_errors() {
        assert!(matches!(classify("adb: No space left on device"), JobError::Fatal(_)));
        assert!(matches!(classify("cp: /sdcard/x: Read-only file system"), JobError::Fatal(_)));
        assert!(matches!(classify("device offline"), JobError::Retry(_)));
    }

    #[test]
    fn summaries() {
        let b = Batch { sent_files: 3, title: "Pixel 8".into(), ..Batch::default() };
        assert_eq!(summary(&b), "3 files sent to Pixel 8");
        let b = Batch {
            sent_files: 1,
            installed: vec!["Foo.apk".into()],
            failed: vec![("big.mkv".into(), "Not enough free space on the phone".into())],
            title: "Moto".into(),
            ..Batch::default()
        };
        assert_eq!(summary(&b), "1 file sent to Moto · Installed Foo.apk on Moto · big.mkv: Not enough free space on the phone");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(12_345_678), "12.3 MB");
    }
}
