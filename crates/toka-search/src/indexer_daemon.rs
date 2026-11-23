//! The indexer daemon: watching one owned-index root per configured folder,
//! generating thumbnails, and publishing a liveness heartbeat for
//! `ts status`. Runs as `toka --indexer` under the Linux systemd user
//! service, and as `ts daemon run` on either platform; this module also
//! holds the Linux-only service helpers.

use crate::daemon_state::DaemonActivity;
use crate::owned_index::{
    OwnedIndex, OwnedIndexError, OwnedIndexWatcher, ScanPhase, WatcherLive, WorkerStage,
};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
#[cfg(not(feature = "e2e"))]
use std::io;
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

#[cfg(not(feature = "e2e"))]
use std::process::ExitStatus;

pub(crate) struct IndexLogger {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl IndexLogger {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
        }
    }

    pub(crate) fn record(&self, message: impl AsRef<str>) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        let _guard = self.write_lock.lock().unwrap();
        if let Err(error) = fs::create_dir_all(parent) {
            eprintln!("Could not create Toka indexer log directory: {error}");
            return;
        }
        let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            eprintln!("Could not open Toka indexer log: {}", self.path.display());
            return;
        };
        if let Err(error) = writeln!(
            file,
            "[{}] {}",
            crate::timestamp::now_rfc3339(),
            message.as_ref()
        ) {
            eprintln!("Could not write Toka indexer log: {error}");
        }
    }
}

#[cfg(not(feature = "e2e"))]
use crate::config::INDEXER_SERVICE_NAME as INDEXER_SERVICE;

#[cfg(not(feature = "e2e"))]
pub(crate) fn ensure_indexer_with_runner<F>(mut run: F) -> Result<(), String>
where
    F: FnMut(&[&str]) -> io::Result<ExitStatus>,
{
    let arguments = ["--user", "start", INDEXER_SERVICE];
    let status = run(&arguments)
        .map_err(|error| format!("Toka could not start its indexer service: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Toka's indexer service could not be started (systemctl exited with {status})"
        ))
    }
}

#[cfg(not(feature = "e2e"))]
pub fn ensure_indexer() {
    if let Err(message) =
        ensure_indexer_with_runner(|arguments| Command::new("systemctl").args(arguments).status())
    {
        eprintln!("{message}");
    }
}

/// Whether the indexer service is running right now, if systemd can say.
#[cfg(all(not(feature = "e2e"), any(target_os = "linux", test)))]
pub(crate) fn indexer_running() -> Option<bool> {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", INDEXER_SERVICE])
        .status()
        .ok()
        .and_then(|status| match status.code() {
            Some(0) => Some(true),
            // `is-active` reports an inactive or failed unit with status 3;
            // anything else (no user bus, signals) means systemd can't say.
            Some(3) => Some(false),
            _ => None,
        })
}

#[cfg(all(feature = "e2e", any(target_os = "linux", test)))]
pub(crate) fn indexer_running() -> Option<bool> {
    None
}

/// Stops the indexer service so it cannot recreate state while it is deleted.
/// The next Toka launch starts it again.
#[cfg(not(feature = "e2e"))]
pub fn stop_indexer() -> Result<(), String> {
    let status = Command::new("systemctl")
        .args(["--user", "stop", INDEXER_SERVICE])
        .status()
        .map_err(|error| format!("could not stop the indexer service: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "the indexer service could not be stopped (systemctl exited with {status})"
        ))
    }
}

/// Where the indexer daemon writes its log: beside the search log.
fn indexer_log_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|directory| {
            directory
                .join(crate::config::APP_LOG_DIR_NAME)
                .join("logs/indexer.log")
        })
        .ok_or_else(|| "Toka could not find a place for its indexer log.".into())
}

/// How often the daemon re-reads the roots table when nothing changes, so a
/// missed filesystem event only delays a folder update.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(10);

/// Minimum gap between reconciliations. A pass touches the watched directory
/// itself (SQLite opens, permission fixes), and worker scan activity does too,
/// so back-to-back events must coalesce instead of each triggering a pass.
const MIN_RECONCILE_INTERVAL: Duration = Duration::from_secs(2);

/// Discards every queued filesystem event. Called after each pass so events
/// the pass itself produced never trigger another one.
fn drain_events<T>(receiver: &mpsc::Receiver<T>) {
    while receiver.try_recv().is_ok() {}
}

/// How much longer to wait before the next pass, so event bursts (including
/// the daemon's own writes) settle instead of reconciling continuously.
fn settle_delay(last_pass: Instant, now: Instant) -> Duration {
    MIN_RECONCILE_INTERVAL.saturating_sub(now.duration_since(last_pass))
}

/// What ended a wait between reconciliations.
#[derive(Debug, PartialEq, Eq)]
enum ReconcileWake {
    /// A filesystem event arrived: reconcile now.
    Event,
    /// The interval elapsed with no events: reconcile anyway.
    Elapsed,
    /// The event sender is gone: the watcher died.
    Gone,
}

/// Waits up to `interval` for a filesystem event, running `heartbeat`
/// every [`crate::daemon_state::HEARTBEAT_INTERVAL`] so liveness stays
/// fresh while nothing changes. Returns what ended the wait.
fn await_reconcile<T>(
    receiver: &mpsc::Receiver<T>,
    interval: Duration,
    mut heartbeat: impl FnMut(),
) -> ReconcileWake {
    let start = Instant::now();
    loop {
        let remaining = interval.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return ReconcileWake::Elapsed;
        }
        match receiver.recv_timeout(remaining.min(crate::daemon_state::HEARTBEAT_INTERVAL)) {
            Ok(_) => return ReconcileWake::Event,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                heartbeat();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return ReconcileWake::Gone,
        }
    }
}

/// Runs the indexer daemon: keeping one owned-index watcher per configured
/// root until the process stops, and publishing a liveness heartbeat for
/// `ts status`. Each pass re-reads the roots table, so folders added or
/// removed while the window is closed apply on the next wake-up.
pub fn run_daemon() -> Result<(), String> {
    let database = crate::owned_index::database_path()?;
    crate::owned_index::ensure_index_parent(&database)?;
    // One supervisor per index: a second one would interleave scan
    // generations with the first and mark partial scans ready.
    let _daemon_lock = try_acquire_daemon_lock(&database)?
        .ok_or_else(|| "Another Toka indexer daemon is already running.".to_owned())?;
    let logger = Arc::new(IndexLogger::new(indexer_log_path()?));
    logger.record("indexer started");
    let (sender, receiver) = mpsc::channel();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .map_err(|error| format!("Toka could not watch its search index: {error}"))?;
    if let Some(parent) = database.parent() {
        // Best effort: the poll below applies folder changes even when the
        // watch fails.
        let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
    }
    let thumbnails = ThumbnailWorker::new(Arc::clone(&logger));
    let mut supervisor = OwnedSupervisor::new(database, Arc::clone(&logger), thumbnails);
    loop {
        supervisor.reconcile();
        // An activity change publishes immediately; the wait below keeps
        // the heartbeat fresh while nothing changes.
        supervisor.publish_heartbeat();
        let last_pass = Instant::now();
        drain_events(&receiver);
        match await_reconcile(&receiver, RECONCILE_INTERVAL, || {
            supervisor.publish_heartbeat()
        }) {
            ReconcileWake::Gone => return Err("Toka's folder watcher stopped unexpectedly.".into()),
            ReconcileWake::Elapsed => {}
            ReconcileWake::Event => {
                drain_events(&receiver);
                std::thread::sleep(settle_delay(last_pass, Instant::now()));
            }
        }
    }
}

/// The lock file guarding one supervisor per index. Held open (and
/// locked) for the supervisor's whole run; dropping it releases the lock,
/// and the operating system releases it on a crash, so the lock can never
/// go stale. The `ts` daemon and the app's in-process watchers take the
/// same lock, so whichever starts second yields to the first.
#[cfg(unix)]
pub(crate) struct DaemonLock {
    _file: fs::File,
}

#[cfg(not(unix))]
pub(crate) struct DaemonLock;

/// Tries the daemon lock without blocking: `Ok(None)` when another
/// supervisor already runs on this index, so the caller yields instead of
/// queueing behind it forever.
#[cfg(unix)]
pub(crate) fn try_acquire_daemon_lock(database: &Path) -> Result<Option<DaemonLock>, String> {
    let path = database.with_file_name("daemon.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| format!("Toka could not lock its indexer daemon: {error}"))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(format!("Toka could not lock its indexer daemon: {error}"));
    }
    Ok(Some(DaemonLock { _file: file }))
}

/// No-op lock where `flock` does not exist: non-Unix platforms run a
/// single supervisor per index by deployment (the service manager starts
/// one), so mutual exclusion is assumed rather than enforced. Liveness
/// assumes the same: `process_is_alive` answers true there, so a fresh
/// heartbeat alone reports the daemon running.
#[cfg(not(unix))]
pub(crate) fn try_acquire_daemon_lock(_database: &Path) -> Result<Option<DaemonLock>, String> {
    Ok(Some(DaemonLock))
}

#[derive(Deserialize)]
struct FindmntOutput {
    filesystems: Vec<FindmntFilesystem>,
}

#[derive(Deserialize)]
struct FindmntFilesystem {
    target: PathBuf,
    source: String,
    uuid: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct MountIdentity {
    pub target: PathBuf,
    pub source: String,
    pub uuid: Option<String>,
}

fn mount_identity_with(program: &Path, path: &Path) -> Option<MountIdentity> {
    let output = Command::new(program)
        .args(["--json", "--target"])
        .arg(path)
        .args(["--output", "TARGET,SOURCE,UUID"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let found: FindmntOutput = serde_json::from_slice(&output.stdout).ok()?;
    let mount = found.filesystems.into_iter().next()?;
    Some(MountIdentity {
        target: mount.target,
        source: mount.source,
        uuid: mount.uuid.filter(|uuid| !uuid.is_empty()),
    })
}

pub(crate) fn mount_identity(path: &Path) -> Option<MountIdentity> {
    mount_identity_with(Path::new("findmnt"), path)
}

/// Whether a search folder is on the storage it was added from. A missing
/// folder is never available; a folder with no recorded mount is available
/// while its path exists, like before mount tracking.
pub(crate) fn path_available(path: &Path, expected: Option<&MountIdentity>) -> bool {
    if !path.is_dir() {
        return false;
    }
    let Some(expected) = expected else {
        return true;
    };
    let Some(current) = mount_identity(path) else {
        return false;
    };
    if current.target != expected.target {
        return false;
    }
    match (&expected.uuid, &current.uuid) {
        (Some(expected), Some(current)) => expected == current,
        _ => expected.source == current.source,
    }
}

#[derive(Clone)]
pub(crate) struct ThumbnailWorker {
    sender: mpsc::Sender<PathBuf>,
    pending: Arc<AtomicU64>,
}

impl ThumbnailWorker {
    pub(crate) fn new(logger: Arc<IndexLogger>) -> Self {
        Self::with_generator(move |folder| {
            crate::thumbnails::generate_folder_with_failures(folder, |video, message| {
                logger.record(format!(
                    "thumbnail generation failed for {video}: {message}",
                    video = video.display()
                ));
            });
        })
    }

    pub(crate) fn with_generator<F>(generator: F) -> Self
    where
        F: Fn(&Path) + Send + 'static,
    {
        let (sender, receiver): (mpsc::Sender<PathBuf>, mpsc::Receiver<PathBuf>) = mpsc::channel();
        let pending = Arc::new(AtomicU64::new(0));
        let worker_pending = Arc::clone(&pending);
        std::thread::Builder::new()
            .name("toka-thumbnail-worker".into())
            .spawn(move || {
                while let Ok(folder) = receiver.recv() {
                    generator(&folder);
                    worker_pending.fetch_sub(1, Ordering::Relaxed);
                }
            })
            .expect("Toka could not start its thumbnail worker");
        Self { sender, pending }
    }

    pub(crate) fn enqueue(&self, folder: &Path) {
        // Reserve first: the worker's matching decrement runs only after
        // this folder is received, so the count can never dip below zero.
        self.pending.fetch_add(1, Ordering::Relaxed);
        if self.sender.send(folder.to_path_buf()).is_err() {
            // The worker is gone; the folder will never generate.
            self.pending.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Folders still waiting for (or undergoing) thumbnail generation.
    pub(crate) fn pending_count(&self) -> u64 {
        self.pending.load(Ordering::Relaxed)
    }
}

/// Publishes the daemon liveness row: identity fixed at startup, activity
/// and queue recomputed from the watchers and the database on every beat.
#[derive(Clone)]
pub(crate) struct DaemonHeartbeat {
    database: PathBuf,
    pid: u32,
    started_at_ns: i64,
}

impl DaemonHeartbeat {
    fn new(database: PathBuf) -> Self {
        Self {
            database,
            pid: std::process::id(),
            started_at_ns: crate::timestamp::now_ns(),
        }
    }
}

/// Keeps one [`OwnedIndexWatcher`] per configured owned-index root: the
/// daemon side of the owned index. The GUI opens the same database without
/// watchers, so the index keeps updating with the window closed.
pub(crate) struct OwnedSupervisor {
    database: PathBuf,
    logger: Arc<IndexLogger>,
    thumbnails: ThumbnailWorker,
    watchers: HashMap<String, ActiveWatcher>,
    last_failure: HashMap<String, Instant>,
    heartbeat: DaemonHeartbeat,
}

/// How long the supervisor waits before restarting a watcher whose worker
/// exited, so a persistently failing scan cannot hot-loop the daemon.
const FAILED_SCAN_RETRY: Duration = Duration::from_secs(60);

/// Whether a watcher may (re)start now: always on a fresh root, only after
/// the cooldown following a worker exit.
fn retry_due(last_failure: Option<Instant>, now: Instant) -> bool {
    last_failure.is_none_or(|failure| now.duration_since(failure) >= FAILED_SCAN_RETRY)
}

/// The path a watcher last reported: the file under the cursor, or the
/// folder being listed while no file is open.
fn current_path(live: &WatcherLive) -> Option<PathBuf> {
    live.last_progress.as_ref().and_then(|progress| {
        progress
            .current_file
            .clone()
            .or_else(|| progress.current_folder.clone())
    })
}

/// Logs a watcher stop failure — unless the worker already exited
/// because its root row is gone. A folder removed mid-pass fails its scan
/// with `FolderNotConfigured` before the reap below runs, so stopping its
/// watcher "fails" with the routine race, not a real stop error, and
/// logging it would blame the shutdown for the removal.
fn log_stop_error(logger: &IndexLogger, path: &Path, error: &OwnedIndexError) {
    if matches!(error, OwnedIndexError::FolderNotConfigured) {
        return;
    }
    logger.record(format!(
        "could not stop the search watcher for {}: {error}",
        path.display()
    ));
}

/// Whether every configured root is offline. An unreadable roots table
/// reads as "not all offline" so the heartbeat still publishes.
fn all_roots_offline(index: &OwnedIndex) -> bool {
    match index.roots_with_state() {
        Ok(roots) => !roots.is_empty() && roots.iter().all(|(_, _, state)| state == "offline"),
        Err(_) => false,
    }
}

struct ActiveWatcher {
    path: PathBuf,
    watcher: OwnedIndexWatcher,
}

impl OwnedSupervisor {
    pub(crate) fn new(
        database: PathBuf,
        logger: Arc<IndexLogger>,
        thumbnails: ThumbnailWorker,
    ) -> Self {
        Self {
            heartbeat: DaemonHeartbeat::new(database.clone()),
            database,
            logger,
            thumbnails,
            watchers: HashMap::new(),
            last_failure: HashMap::new(),
        }
    }

    /// Republishes the daemon liveness row from the watchers' live states
    /// and the database's queued work. Best-effort: a transient failure
    /// (a locked database mid-scan) is logged, never fatal, and the next
    /// beat retries. Publishing creates the database when no folder was
    /// ever added, so a running daemon always has a row to read.
    pub(crate) fn publish_heartbeat(&self) {
        let index = match OwnedIndex::open(&self.heartbeat.database) {
            Ok(index) => index,
            Err(error) => {
                self.logger
                    .record(format!("could not publish the daemon heartbeat: {error}"));
                return;
            }
        };
        let (activity, current_root, current_path) = self.current_activity(&index);
        let queue = match index.daemon_queue_counts(self.thumbnails.pending_count()) {
            Ok(queue) => queue,
            Err(error) => {
                self.logger
                    .record(format!("could not publish the daemon heartbeat: {error}"));
                return;
            }
        };
        let beat = crate::daemon_state::DaemonBeat {
            heartbeat_at_ns: crate::timestamp::now_ns(),
            activity,
            current_root,
            current_path,
            queue,
        };
        if let Err(error) =
            index.publish_daemon_state(self.heartbeat.pid, self.heartbeat.started_at_ns, &beat)
        {
            self.logger
                .record(format!("could not publish the daemon heartbeat: {error}"));
        }
    }

    /// What the daemon is doing right now: a scanning watcher wins over a
    /// repairing one, thumbnails over idleness, and configured-but-missing
    /// roots report waiting rather than idle. With several roots active at
    /// once, any one of them may be reported; the queue totals still cover
    /// all of them.
    fn current_activity(
        &self,
        index: &OwnedIndex,
    ) -> (DaemonActivity, Option<PathBuf>, Option<PathBuf>) {
        let mut repairing = None;
        for entry in self.watchers.values() {
            // A worker that already exited reports nothing: the next pass
            // reaps it, but this beat must not claim its stale stage.
            if entry.watcher.is_finished() {
                continue;
            }
            let live = entry.watcher.live();
            match live.stage {
                WorkerStage::Scanning => {
                    return (
                        DaemonActivity::Scanning,
                        Some(entry.path.clone()),
                        current_path(&live),
                    );
                }
                WorkerStage::Repairing => {
                    if repairing.is_none() {
                        repairing = Some((entry.path.clone(), current_path(&live)));
                    }
                }
                WorkerStage::Watching => {}
            }
        }
        if let Some((root, path)) = repairing {
            return (DaemonActivity::Repairing, Some(root), path);
        }
        if self.thumbnails.pending_count() > 0 {
            return (DaemonActivity::Thumbnails, None, None);
        }
        if all_roots_offline(index) {
            return (DaemonActivity::OfflineWaiting, None, None);
        }
        (DaemonActivity::Idle, None, None)
    }

    /// Starts watchers for newly configured roots and stops watchers whose
    /// root went away, reading the roots table fresh every time so a wake-up
    /// always picks up the latest configuration. Does nothing until the owned
    /// database exists, so it stays out of the way until folders are added.
    pub(crate) fn reconcile(&mut self) {
        match std::fs::metadata(&self.database) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.stop_all();
                return;
            }
            Err(error) => {
                self.logger.record(format!(
                    "owned index watcher pass failed: cannot read index metadata: {error}"
                ));
                self.stop_all();
                return;
            }
            Ok(metadata) if !metadata.is_file() => {
                self.logger.record(format!(
                    "owned index watcher pass failed: index path is not a file: {}",
                    self.database.display()
                ));
                self.stop_all();
                return;
            }
            Ok(_) => {}
        }
        let index = match OwnedIndex::open(&self.database) {
            Ok(index) => index,
            Err(error) => {
                self.logger
                    .record(format!("owned index watcher pass failed: {error}"));
                return;
            }
        };
        let roots = match index.roots_with_state() {
            Ok(roots) => roots,
            Err(error) => {
                self.logger
                    .record(format!("owned index watcher pass failed: {error}"));
                return;
            }
        };
        let live: HashSet<&str> = roots.iter().map(|(id, _, _)| id.as_str()).collect();
        let gone: Vec<String> = self
            .watchers
            .keys()
            .filter(|id| !live.contains(id.as_str()))
            .cloned()
            .collect();
        for id in gone {
            if let Some(entry) = self.watchers.remove(&id) {
                if let Err(error) = entry.watcher.stop() {
                    log_stop_error(&self.logger, &entry.path, &error);
                }
            }
            self.last_failure.remove(&id);
        }
        for (id, path, stored) in roots {
            self.reap_if_finished(&id);
            let expected = match index.mount_columns(&id) {
                Ok(stored) => stored.target.map(|target| MountIdentity {
                    target,
                    source: stored.source.unwrap_or_default(),
                    uuid: stored.uuid,
                }),
                Err(error) => {
                    self.logger.record(format!(
                        "could not read the mount for {}: {error}",
                        path.display()
                    ));
                    continue;
                }
            };
            if !path_available(&path, expected.as_ref()) {
                if let Some(entry) = self.watchers.remove(&id) {
                    if let Err(error) = entry.watcher.stop() {
                        log_stop_error(&self.logger, &entry.path, &error);
                    }
                }
                // One root's write failure must not skip the rest: log it
                // and move on, so a single bad row cannot stall every
                // folder behind it.
                if stored != "offline" {
                    if let Err(error) = self.set_root_state(&id, "offline") {
                        self.logger.record(format!(
                            "could not mark {} offline: {error}",
                            path.display()
                        ));
                    }
                }
                continue;
            }
            if stored == "offline" {
                if let Err(error) = self.set_root_state(&id, "pending") {
                    self.logger.record(format!(
                        "could not mark {} pending: {error}",
                        path.display()
                    ));
                }
            }
            let same = self
                .watchers
                .get(&id)
                .is_some_and(|entry| entry.path == path);
            if same {
                continue;
            }
            if let Some(entry) = self.watchers.remove(&id) {
                let _ = entry.watcher.stop();
            }
            // A watcher whose worker exited restarts only after a cooldown,
            // so a persistently failing scan cannot hot-loop the daemon.
            if !retry_due(self.last_failure.get(&id).copied(), Instant::now()) {
                continue;
            }
            if expected.is_none() {
                let current = mount_identity(&path);
                if let Err(error) = self.record_mount(
                    &id,
                    current.as_ref().map(|mount| mount.target.as_path()),
                    current.as_ref().map(|mount| mount.source.as_str()),
                    current.as_ref().and_then(|mount| mount.uuid.as_deref()),
                ) {
                    // As above: a failed mount write skips this root, not
                    // the rest of the pass.
                    self.logger.record(format!(
                        "could not record the mount for {}: {error}",
                        path.display()
                    ));
                    continue;
                }
            }
            let thumbnails = self.thumbnails.clone();
            let logger = Arc::clone(&self.logger);
            let root = path.clone();
            match OwnedIndexWatcher::start_with_progress(
                &self.database,
                id.clone(),
                path.clone(),
                move |progress| {
                    if progress.phase == ScanPhase::Ready {
                        thumbnails.enqueue(&root);
                        logger.record(format!(
                            "indexing complete for owned folder: {}",
                            root.display()
                        ));
                    }
                },
            ) {
                Ok(watcher) => {
                    self.watchers.insert(id, ActiveWatcher { path, watcher });
                }
                Err(error) => {
                    self.logger.record(format!(
                        "could not watch the search folder {}: {error}",
                        path.display()
                    ));
                }
            }
        }
    }

    /// Removes the watcher for `id` when its worker thread already exited,
    /// logging why it stopped so the next pass can restart it. A finished
    /// watcher indexes nothing, so leaving it in place would stall the root
    /// until the daemon restarts.
    fn reap_if_finished(&mut self, id: &str) {
        let finished = self
            .watchers
            .get(id)
            .is_some_and(|entry| entry.watcher.is_finished());
        if !finished {
            return;
        }
        let entry = self.watchers.remove(id).expect("watcher present");
        match entry.watcher.stop() {
            Ok(()) => self.logger.record(format!(
                "search watcher for {} exited unexpectedly",
                entry.path.display()
            )),
            Err(error) => {
                self.logger.record(format!(
                    "search watcher for {} exited: {error}",
                    entry.path.display()
                ));
                self.last_failure.insert(id.to_owned(), Instant::now());
            }
        }
    }

    fn set_root_state(&self, id: &str, state: &str) -> Result<(), String> {
        match OwnedIndex::open(&self.database) {
            Ok(mut index) => index
                .set_root_state(id, state)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn record_mount(
        &self,
        id: &str,
        target: Option<&Path>,
        source: Option<&str>,
        uuid: Option<&str>,
    ) -> Result<(), String> {
        match OwnedIndex::open(&self.database) {
            Ok(mut index) => index
                .record_mount(id, target, source, uuid)
                .map_err(|error| error.to_string()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn stop_all(&mut self) {
        for (_, entry) in self.watchers.drain() {
            if let Err(error) = entry.watcher.stop() {
                log_stop_error(&self.logger, &entry.path, &error);
            }
        }
    }

    #[cfg(test)]
    fn active_count(&self) -> usize {
        self.watchers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owned_index::OwnedIndexManager;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    fn test_supervisor(database: PathBuf, thumbnails: ThumbnailWorker) -> OwnedSupervisor {
        OwnedSupervisor::new(
            database,
            Arc::new(IndexLogger::new(PathBuf::from("/nonexistent-indexer.log"))),
            thumbnails,
        )
    }

    fn quiet_thumbnails() -> ThumbnailWorker {
        ThumbnailWorker::with_generator(|_| {})
    }

    fn poll_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        check()
    }

    fn write_media(root: &Path, name: &str) {
        fs::write(root.join(name), b"media").unwrap();
    }

    #[test]
    fn reconcile_wake_ups_settle_instead_of_triggering_passes() {
        let last_pass = Instant::now();
        assert_eq!(settle_delay(last_pass, last_pass), MIN_RECONCILE_INTERVAL);
        assert_eq!(
            settle_delay(last_pass, last_pass + Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        assert_eq!(
            settle_delay(last_pass, last_pass + MIN_RECONCILE_INTERVAL),
            Duration::ZERO
        );

        let (sender, receiver) = mpsc::channel();
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        drain_events(&receiver);
        assert!(receiver.try_recv().is_err());
    }

    #[cfg(not(feature = "e2e"))]
    #[test]
    fn starts_the_user_indexer_service_when_toka_launches() {
        let mut commands: Vec<Vec<String>> = Vec::new();

        ensure_indexer_with_runner(|arguments| {
            commands.push(
                arguments
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
            );
            std::process::Command::new("true").status()
        })
        .unwrap();

        assert_eq!(
            commands,
            vec![vec![
                "--user".to_owned(),
                "start".to_owned(),
                "toka-indexer.service".to_owned(),
            ]]
        );
    }

    #[test]
    fn reconcile_starts_a_watcher_and_rows_appear() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());

        let mut supervisor = test_supervisor(database, quiet_thumbnails());
        // Nothing configured yet: a wake-up with an empty database is a no-op.
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 0);

        let added = manager.add_folder(&media).unwrap();
        let id = added.folders[0].id.clone();
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 1);

        assert!(
            poll_until(Duration::from_secs(10), || {
                manager.state().unwrap().indexed_videos == 1
            }),
            "expected the watcher to index the new folder"
        );

        // Reconciling again neither restarts the watcher nor rescans.
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 1);
        assert_eq!(manager.state().unwrap().indexed_videos, 1);

        manager.remove_folder(&id).unwrap();
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 0);
    }

    #[test]
    fn missing_roots_are_marked_offline() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        manager.add_folder(&media).unwrap();

        let mut supervisor = test_supervisor(database, quiet_thumbnails());
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 1);

        fs::remove_dir_all(&media).unwrap();
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 0);
        let state = manager.state().unwrap();
        assert_eq!(
            state.folders[0].status,
            crate::owned_index::OwnedFolderStatus::Offline
        );
    }

    #[test]
    fn reconcile_stops_watchers_and_logs_when_the_database_is_unreadable() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        manager.add_folder(&media).unwrap();
        let log = directory.path().join("indexer.log");
        let mut supervisor = OwnedSupervisor::new(
            database.clone(),
            Arc::new(IndexLogger::new(log.clone())),
            quiet_thumbnails(),
        );
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 1);

        // A directory at the database path is a broken setup, not a
        // missing index: watchers stop and the pass is logged.
        fs::remove_file(&database).unwrap();
        fs::create_dir(&database).unwrap();
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 0);
        let log_text = fs::read_to_string(&log).unwrap();
        assert!(log_text.contains("index path is not a file"), "{log_text}");
    }

    #[test]
    fn reconcile_logs_metadata_failures_instead_of_reading_them_as_missing() {
        let directory = tempdir().unwrap();
        // Overlong names fail metadata deterministically, even as root.
        let database = directory.path().join("x".repeat(300));
        let log = directory.path().join("indexer.log");
        let mut supervisor = OwnedSupervisor::new(
            database,
            Arc::new(IndexLogger::new(log.clone())),
            quiet_thumbnails(),
        );

        supervisor.reconcile();

        assert_eq!(supervisor.active_count(), 0);
        let log_text = fs::read_to_string(&log).unwrap();
        assert!(
            log_text.contains("cannot read index metadata"),
            "{log_text}"
        );
    }

    #[test]
    fn completed_scans_enqueue_thumbnails() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        manager.add_folder(&media).unwrap();

        let (sender, receiver) = mpsc::channel();
        let thumbnails = ThumbnailWorker::with_generator(move |folder| {
            let _ = sender.send(folder.to_path_buf());
        });
        let mut supervisor = test_supervisor(database, thumbnails);
        supervisor.reconcile();

        let scanned = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(scanned, media.canonicalize().unwrap());
    }

    #[test]
    fn failed_scan_retries_only_after_a_cooldown() {
        let now = Instant::now();
        assert!(retry_due(None, now));
        assert!(!retry_due(Some(now), now));
        assert!(retry_due(
            Some(now - FAILED_SCAN_RETRY - Duration::from_secs(1)),
            now
        ));
    }

    #[test]
    fn finished_workers_are_reaped_with_backoff() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        let id = manager.add_folder(&media).unwrap().folders[0].id.clone();

        // A watcher whose worker already exited. It scans a database with no
        // row for this root, so begin_scan fails closed and the worker exits.
        // Permission fixtures cannot produce this: the stat-only scanner
        // reads a locked file fine, and an unreadable subdirectory fails
        // recursive watch startup before any scan runs.
        let ghost = directory.path().join("ghost.sqlite3");
        let watcher =
            OwnedIndexWatcher::start_with_progress(&ghost, id.clone(), media.clone(), |_| {})
                .unwrap();
        assert!(
            poll_until(Duration::from_secs(10), || watcher.is_finished()),
            "expected the worker to exit on its missing root row"
        );

        let mut supervisor = test_supervisor(database, quiet_thumbnails());
        supervisor.watchers.insert(
            id,
            ActiveWatcher {
                path: media,
                watcher,
            },
        );
        supervisor.reconcile();
        assert_eq!(
            supervisor.active_count(),
            0,
            "expected the exited worker to be reaped"
        );
        // Still backing off: no immediate restart loop.
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 0);
    }

    #[test]
    fn stop_errors_skip_the_unconfigured_root_race() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        let id = manager.add_folder(&media).unwrap().folders[0].id.clone();

        // A database with the schema but no row for this root: the
        // worker fails closed with FolderNotConfigured and exits, as a
        // folder removed mid-pass does.
        let ghost = directory.path().join("ghost.sqlite3");
        OwnedIndex::open(&ghost).unwrap();
        let failed =
            OwnedIndexWatcher::start_with_progress(&ghost, id.clone(), media.clone(), |_| {})
                .unwrap();
        assert!(
            poll_until(Duration::from_secs(10), || failed.is_finished()),
            "expected the worker to exit on its missing root row"
        );
        // Sanity for the assertion below: stopping really does surface
        // the routine race.
        assert!(matches!(
            failed.stop(),
            Err(crate::owned_index::OwnedIndexError::FolderNotConfigured)
        ));

        let twin =
            OwnedIndexWatcher::start_with_progress(&ghost, id.clone(), media.clone(), |_| {})
                .unwrap();
        assert!(
            poll_until(Duration::from_secs(10), || twin.is_finished()),
            "expected the twin worker to exit the same way"
        );
        let log = directory.path().join("indexer.log");
        let mut supervisor = OwnedSupervisor::new(
            database,
            Arc::new(IndexLogger::new(log.clone())),
            quiet_thumbnails(),
        );
        supervisor.watchers.insert(
            id,
            ActiveWatcher {
                path: media.clone(),
                watcher: twin,
            },
        );
        supervisor.stop_all();

        let logged = fs::read_to_string(&log).unwrap_or_default();
        assert!(
            !logged.contains("could not stop"),
            "the routine race must not log a stop error: {logged}"
        );

        // A genuine stop failure still logs: only the unconfigured
        // race is skipped.
        let log = directory.path().join("indexer-direct.log");
        let logger = IndexLogger::new(log.clone());
        log_stop_error(
            &logger,
            Path::new("/media"),
            &crate::owned_index::OwnedIndexError::WorkerStopped,
        );
        let logged = fs::read_to_string(&log).unwrap_or_default();
        assert!(
            logged.contains("could not stop the search watcher for /media"),
            "a genuine stop failure must still log: {logged}"
        );
    }

    #[test]
    fn reconcile_continues_past_a_failing_root_write() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        write_media(&first, "a.mp4");
        write_media(&second, "b.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        manager.add_folder(&first).unwrap();
        manager.add_folder(&second).unwrap();

        // Roots reconcile in id order; fail the first one's writes so a
        // break would skip the second root entirely.
        let roots = OwnedIndex::open(&database)
            .unwrap()
            .roots_with_state()
            .unwrap();
        assert_eq!(roots.len(), 2);
        let failing = roots[0].0.clone();
        let healthy = roots[1].0.clone();
        let trigger = format!(
            "CREATE TRIGGER fail_first_root BEFORE UPDATE ON roots
             WHEN NEW.id = '{failing}'
             BEGIN SELECT RAISE(ABORT, 'injected write failure'); END"
        );
        rusqlite::Connection::open(&database)
            .unwrap()
            .execute_batch(&trigger)
            .unwrap();

        let mut supervisor = test_supervisor(database, quiet_thumbnails());
        supervisor.reconcile();

        assert!(
            supervisor.watchers.contains_key(&healthy),
            "a failing root must not skip the rest of the pass"
        );
        assert!(!supervisor.watchers.contains_key(&failing));
    }

    #[test]
    fn waiting_for_reconcile_returns_on_events_timeouts_and_disconnects() {
        // An event ends the wait immediately, with no heartbeat due yet.
        let (sender, receiver) = mpsc::channel();
        sender.send(1).unwrap();
        let mut beats = 0;
        assert_eq!(
            await_reconcile(&receiver, Duration::from_secs(10), || beats += 1),
            ReconcileWake::Event
        );
        assert_eq!(beats, 0);

        // Silence heartbeats while waiting, then reports the elapsed
        // interval.
        let (_sender, receiver): (mpsc::Sender<u8>, _) = mpsc::channel();
        let mut beats = 0;
        assert_eq!(
            await_reconcile(&receiver, Duration::from_millis(100), || beats += 1),
            ReconcileWake::Elapsed
        );
        assert_eq!(beats, 1);

        // A dead sender ends the wait without blocking for the interval.
        let (sender, receiver) = mpsc::channel::<u8>();
        drop(sender);
        assert_eq!(
            await_reconcile(&receiver, Duration::from_secs(10), || {}),
            ReconcileWake::Gone
        );
    }

    #[test]
    fn reconcile_waits_heartbeat_on_a_two_second_cadence() {
        // A quiet 4.5-second wait heartbeats about twice (every
        // HEARTBEAT_INTERVAL) and then reports the elapsed interval.
        // Bounds are generous: scheduling jitter may steal or delay a
        // beat, but a dead cadence or a hot loop fails loudly.
        let (_sender, receiver) = mpsc::channel::<u8>();
        let mut beats = 0;
        let wake = await_reconcile(&receiver, Duration::from_millis(4500), || beats += 1);
        assert_eq!(wake, ReconcileWake::Elapsed);
        assert!((1..=3).contains(&beats), "expected ~2 beats, got {beats}");
    }

    #[test]
    fn thumbnail_backlog_counts_folders_until_they_generate() {
        let (started, started_receiver) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel::<()>();
        let thumbnails = ThumbnailWorker::with_generator(move |folder| {
            let _ = started.send(folder.to_path_buf());
            let _ = release_receiver.recv();
        });
        assert_eq!(thumbnails.pending_count(), 0);

        thumbnails.enqueue(Path::new("/media/one"));
        thumbnails.enqueue(Path::new("/media/two"));
        assert_eq!(thumbnails.pending_count(), 2);

        // The first folder is generating (blocked on release); both count.
        assert_eq!(
            started_receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            PathBuf::from("/media/one")
        );
        assert_eq!(thumbnails.pending_count(), 2);

        release.send(()).unwrap();
        assert_eq!(
            started_receiver
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            PathBuf::from("/media/two")
        );
        assert!(
            poll_until(Duration::from_secs(10), || thumbnails.pending_count() == 1),
            "expected the finished folder to leave the backlog"
        );
        release.send(()).unwrap();
        assert!(
            poll_until(Duration::from_secs(10), || thumbnails.pending_count() == 0),
            "expected the backlog to drain"
        );
    }

    #[test]
    fn heartbeat_publishes_liveness_and_follows_a_scan_to_idle() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        manager.add_folder(&media).unwrap();

        let mut supervisor = test_supervisor(database.clone(), quiet_thumbnails());
        // No watchers yet: idle with one folder still to index.
        supervisor.publish_heartbeat();
        let status = crate::daemon_state::read_status(&database).unwrap();
        assert!(status.running);
        assert_eq!(status.pid, Some(std::process::id()));
        assert_eq!(
            status.activity,
            Some(crate::daemon_state::DaemonActivity::Idle)
        );
        assert!(status.uptime_secs.unwrap() <= 2);
        assert!(status.heartbeat_age_secs.unwrap() <= 2);
        assert_eq!(status.queue.pending_roots, 1);

        supervisor.reconcile();
        assert!(
            poll_until(Duration::from_secs(10), || {
                manager.state().unwrap().indexed_videos == 1
            }),
            "expected the watcher to index the new folder"
        );
        // The committed count lands before the worker finishes: it may
        // still be marking the run ready or waiting on thumbnail
        // generation. Only a settled worker (watching, nothing pending)
        // reads idle.
        assert!(
            poll_until(Duration::from_secs(10), || {
                supervisor
                    .watchers
                    .values()
                    .next()
                    .is_some_and(|entry| entry.watcher.live().stage == WorkerStage::Watching)
                    && supervisor.thumbnails.pending_count() == 0
            }),
            "expected the watcher to settle to watching with no thumbnails pending"
        );
        supervisor.publish_heartbeat();
        let status = crate::daemon_state::read_status(&database).unwrap();
        assert_eq!(
            status.activity,
            Some(crate::daemon_state::DaemonActivity::Idle)
        );
        assert!(status.queue.is_empty());
    }

    #[test]
    fn heartbeat_reports_a_scan_in_progress() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        // Enough files that the scan observably lasts: the test must catch
        // it mid-flight, and a loaded CI worker needs a wide window.
        for number in 0..2000 {
            write_media(&media, &format!("clip-{number:05}.mp4"));
        }
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        let root_id = manager.add_folder(&media).unwrap().folders[0].id.clone();

        // Gate the worker after two batch commits instead of racing a
        // poll against the scan: deterministic on any machine speed. The
        // gate only ever trips for this root id.
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc,
        };
        let (gated_tx, gated_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let committed = Arc::new(AtomicUsize::new(0));
        let worker_committed = Arc::clone(&committed);
        crate::owned_index::with_global_sync_probe(
            &root_id,
            move |site| {
                if site == crate::owned_index::SyncProbeSite::ScanBatchCommitted
                    && worker_committed.fetch_add(1, Ordering::Relaxed) + 1 == 2
                {
                    gated_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(60))
                        .expect("the test releases the gate after asserting the heartbeat");
                }
            },
            || {
                let mut supervisor = test_supervisor(database.clone(), quiet_thumbnails());
                supervisor.reconcile();
                assert_eq!(supervisor.active_count(), 1);

                gated_rx
                    .recv_timeout(Duration::from_secs(60))
                    .expect("the scan must commit two batches");
                // Worker paused mid-scan: the heartbeat must show it.
                supervisor.publish_heartbeat();
                let status = crate::daemon_state::read_status(&database).unwrap();
                assert!(status.running);
                assert_eq!(
                    status.activity,
                    Some(crate::daemon_state::DaemonActivity::Scanning)
                );
                assert_eq!(status.current_root, Some(media.canonicalize().unwrap()));
                let current = status.current_path.expect("current file");
                assert!(current.starts_with(media.canonicalize().unwrap()));
                assert_eq!(status.queue.pending_roots, 1);

                release_tx.send(()).unwrap();
                // The gated scan completes normally once released.
                assert!(
                    poll_until(Duration::from_secs(60), || {
                        manager
                            .state()
                            .is_ok_and(|state| state.indexed_videos == 2000)
                    }),
                    "the released scan never finished"
                );
            },
        );
    }

    #[test]
    fn heartbeat_ignores_watchers_whose_workers_exited() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        let id = manager.add_folder(&media).unwrap().folders[0].id.clone();

        // A watcher whose worker already exited on its missing root row,
        // stuck reporting the scan stage it died in.
        let ghost = directory.path().join("ghost.sqlite3");
        let watcher =
            OwnedIndexWatcher::start_with_progress(&ghost, id.clone(), media.clone(), |_| {})
                .unwrap();
        assert!(
            poll_until(Duration::from_secs(10), || watcher.is_finished()),
            "expected the worker to exit on its missing root row"
        );

        let mut supervisor = test_supervisor(database.clone(), quiet_thumbnails());
        supervisor.watchers.insert(
            id,
            ActiveWatcher {
                path: media,
                watcher,
            },
        );
        supervisor.publish_heartbeat();

        let status = crate::daemon_state::read_status(&database).unwrap();
        assert_eq!(
            status.activity,
            Some(crate::daemon_state::DaemonActivity::Idle)
        );
        assert_eq!(status.queue.pending_roots, 1);
    }

    #[test]
    fn heartbeat_reports_waiting_when_every_root_is_offline() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        manager.add_folder(&media).unwrap();

        let mut supervisor = test_supervisor(database.clone(), quiet_thumbnails());
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 1);
        fs::remove_dir_all(&media).unwrap();
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 0);

        supervisor.publish_heartbeat();
        let status = crate::daemon_state::read_status(&database).unwrap();
        assert!(status.running);
        assert_eq!(
            status.activity,
            Some(crate::daemon_state::DaemonActivity::OfflineWaiting)
        );
        // Offline is not queued work: the root counts apart from the
        // pending total, which is empty.
        assert_eq!(status.queue.pending_roots, 0);
        assert!(status.queue.is_empty());
        assert_eq!(status.offline_roots, 1);
    }

    #[test]
    fn heartbeat_reports_thumbnail_work_while_scans_are_idle() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        manager.add_folder(&media).unwrap();

        let (release, release_receiver) = mpsc::channel::<()>();
        let thumbnails = ThumbnailWorker::with_generator(move |_| {
            let _ = release_receiver.recv();
        });
        let mut supervisor = test_supervisor(database.clone(), thumbnails);
        supervisor.reconcile();
        // The scan completes and enqueues thumbnails while the generator
        // stays blocked: scans idle, backlog outstanding.
        assert!(
            poll_until(Duration::from_secs(10), || {
                supervisor
                    .watchers
                    .values()
                    .next()
                    .is_some_and(|entry| entry.watcher.live().stage == WorkerStage::Watching)
                    && supervisor.thumbnails.pending_count() == 1
            }),
            "expected the scan to finish with thumbnails pending"
        );

        supervisor.publish_heartbeat();
        let status = crate::daemon_state::read_status(&database).unwrap();
        assert_eq!(
            status.activity,
            Some(crate::daemon_state::DaemonActivity::Thumbnails)
        );
        assert_eq!(status.queue.pending_thumbnails, 1);

        release.send(()).unwrap();
        assert!(
            poll_until(Duration::from_secs(10), || supervisor
                .thumbnails
                .pending_count()
                == 0),
            "expected the thumbnail backlog to drain"
        );
        supervisor.publish_heartbeat();
        let status = crate::daemon_state::read_status(&database).unwrap();
        assert_eq!(
            status.activity,
            Some(crate::daemon_state::DaemonActivity::Idle)
        );
    }

    #[test]
    fn stats_reports_the_indexer_service_state() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database);
        manager.add_folder(&media).unwrap();

        assert_eq!(manager.stats().unwrap().service_running, indexer_running());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_mismatch_marks_the_root_offline() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        let id = manager.add_folder(&media).unwrap().folders[0].id.clone();

        // Whatever storage the test machine has, this is not it.
        OwnedIndex::open(&database)
            .unwrap()
            .record_mount(
                &id,
                Some(Path::new("/nonexistent-mount-target")),
                Some("nonexistent-source"),
                None,
            )
            .unwrap();

        let mut supervisor = test_supervisor(database, quiet_thumbnails());
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 0);
        assert_eq!(
            manager.state().unwrap().folders[0].status,
            crate::owned_index::OwnedFolderStatus::Offline
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_is_recorded_on_first_reconcile() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        let id = manager.add_folder(&media).unwrap().folders[0].id.clone();

        let mut supervisor = test_supervisor(database.clone(), quiet_thumbnails());
        supervisor.reconcile();

        let stored = OwnedIndex::open(&database)
            .unwrap()
            .mount_columns(&id)
            .unwrap();
        assert!(
            stored.target.is_some(),
            "expected findmnt to identify the test directory's mount"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mount_identity_parses_findmnt_output() {
        let directory = tempdir().unwrap();
        let fake = directory.path().join("findmnt");
        fs::write(
            &fake,
            "#!/bin/sh\necho '{\"filesystems\":[{\"target\":\"/mnt/x\",\"source\":\"/dev/sda1\",\"uuid\":\"\"}]}'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

        let mount = mount_identity_with(&fake, Path::new("/mnt/x/sub")).expect("fake findmnt");
        assert_eq!(mount.target, PathBuf::from("/mnt/x"));
        assert_eq!(mount.source, "/dev/sda1");
        assert_eq!(mount.uuid, None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn path_available_trusts_a_matching_mount() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();

        let Some(current) = mount_identity(&media) else {
            // No findmnt on this machine: nothing to compare against.
            return;
        };
        assert!(path_available(&media, Some(&current)));
        assert!(path_available(&media, None));
        let mut moved = current.clone();
        moved.target = PathBuf::from("/nonexistent-mount-target");
        assert!(!path_available(&media, Some(&moved)));
        assert!(!path_available(&media.join("missing"), Some(&current)));
    }
}
