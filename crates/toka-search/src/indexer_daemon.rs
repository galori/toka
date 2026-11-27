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
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

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

#[cfg(all(not(feature = "e2e"), any(target_os = "linux", test)))]
use crate::config::INDEXER_SERVICE_NAME as INDEXER_SERVICE;

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

/// Where the indexer daemon writes its log: beside the search log on Linux,
/// under `~/Library/Application Support` on macOS — outside the search-log
/// folder purge removes wholesale there, so purge names it separately.
pub(crate) fn indexer_log_path() -> Result<PathBuf, String> {
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

/// How often the daemon recounts the most used tags. Counts shift slowly, and
/// a recount reads every file's tags, so minutes apart is plenty.
const TOP_TAGS_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Whether the top tags are due for a recount: never counted, or last counted
/// at least [`TOP_TAGS_INTERVAL`] ago.
fn top_tags_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.duration_since(last) >= TOP_TAGS_INTERVAL)
}

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
    let top_tags_database = database.clone();
    let mut supervisor = OwnedSupervisor::new(database, Arc::clone(&logger), thumbnails);
    let mut top_tags_counted: Option<Instant> = None;
    let top_tags_running = Arc::new(AtomicBool::new(false));
    loop {
        supervisor.reconcile();
        let now = Instant::now();
        if top_tags_due(top_tags_counted, now) {
            top_tags_counted = Some(now);
            // On its own thread, so a big index never stalls reconciling or
            // the heartbeat. Best effort: the app counts on demand when no
            // list exists.
            if !top_tags_running.swap(true, Ordering::AcqRel) {
                let database = top_tags_database.clone();
                let logger = Arc::clone(&logger);
                let running = Arc::clone(&top_tags_running);
                std::thread::spawn(move || {
                    if let Err(error) = crate::top_tags::refresh(&database) {
                        logger.record(format!("top tags not refreshed: {error}"));
                    }
                    running.store(false, Ordering::Release);
                });
            }
        }
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
    _primary: fs::File,
    _legacy: fs::File,
}

#[cfg(not(unix))]
pub(crate) struct DaemonLock;

/// Where the primary daemon lock lives: a runtime directory, never the
/// index folder. `TOKA_RUNTIME_DIR` wins when set (tests and isolated runs
/// point it at scratch); otherwise Linux uses `XDG_RUNTIME_DIR/toka`, and
/// macOS — like the Linux fallback — uses the cache folder's `toka`
/// directory, which no reset deletes (resets delete `app.toka.desktop`,
/// never `toka`). A missing home falls back to a per-user directory under
/// the temporary directory, since that fallback is shared across users.
#[cfg(unix)]
pub(crate) fn daemon_runtime_dir() -> PathBuf {
    let override_dir = std::env::var_os("TOKA_RUNTIME_DIR").map(PathBuf::from);
    #[cfg(target_os = "linux")]
    let xdg_runtime = dirs::runtime_dir();
    #[cfg(not(target_os = "linux"))]
    let xdg_runtime: Option<PathBuf> = None;
    resolve_runtime_dir(
        override_dir,
        xdg_runtime,
        dirs::cache_dir(),
        std::env::temp_dir(),
        current_uid(),
    )
}

/// [`daemon_runtime_dir`] with the environment passed in, so the
/// precedence is unit-testable without touching the process environment.
/// The temporary-directory fallback carries the user id: without it every
/// user would share one `$TMPDIR/toka`, and the first to create it would
/// lock the rest out.
#[cfg(unix)]
fn resolve_runtime_dir(
    override_dir: Option<PathBuf>,
    xdg_runtime: Option<PathBuf>,
    cache: Option<PathBuf>,
    temp: PathBuf,
    uid: u32,
) -> PathBuf {
    if let Some(dir) = override_dir {
        return dir;
    }
    #[cfg(target_os = "linux")]
    if let Some(dir) = xdg_runtime {
        return dir.join("toka");
    }
    #[cfg(not(target_os = "linux"))]
    let _ = xdg_runtime;
    if let Some(dir) = cache {
        return dir.join("toka");
    }
    temp.join(format!("toka-{uid}"))
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: `getuid` takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

/// Creates the runtime directory and pins it to this user: mode 0700, not
/// a symlink, owned by `uid`. The temporary-directory fallback is world
/// writable, so without the ownership check another user's pre-created
/// directory — or pre-created lock files — would lock this user out or
/// collect their locks.
///
/// Residual TOCTOU, documented rather than rebuilt: a rename-swap after
/// the final verification still redirects the locks taken next. It needs
/// a world-writable temporary parent, a local attacker, and microsecond
/// timing — and it only misplaces locks (denial of service), never
/// grants access — so fd-pinning the directory was judged
/// disproportionate. Unexploitable deterministically in-test, hence no
/// test.
#[cfg(unix)]
fn ensure_runtime_dir(dir: &Path, uid: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    if fs::symlink_metadata(dir).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(format!(
            "Toka could not lock its indexer daemon: {} is a symlink",
            dir.display()
        ));
    }
    fs::create_dir_all(dir)
        .map_err(|error| format!("Toka could not lock its indexer daemon: {error}"))?;
    // The pre-check above races a concurrent swap — creation and chmod
    // both follow links — so the directory is verified AFTER creation:
    // what exists now must be this user's real directory, never a link
    // planted between the two calls.
    verified_runtime_metadata(dir, uid)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("Toka could not lock its indexer daemon: {error}"))?;
    // And once more after the chmod, so a swap between the chmod and the
    // first verification cannot hand back a link with trusted contents.
    let metadata = verified_runtime_metadata(dir, uid)?;
    if metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(format!(
            "Toka could not lock its indexer daemon: {} changed while it was secured",
            dir.display()
        ));
    }
    Ok(())
}

/// The runtime directory this instant: a real directory (never a link)
/// owned by `uid`. Called after every filesystem step above, so a swap
/// between the steps cannot redirect the locks.
#[cfg(unix)]
fn verified_runtime_metadata(dir: &Path, uid: u32) -> Result<fs::Metadata, String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(dir)
        .map_err(|error| format!("Toka could not lock its indexer daemon: {error}"))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "Toka could not lock its indexer daemon: {} is a symlink",
            dir.display()
        ));
    }
    if metadata.uid() != uid {
        return Err(format!(
            "Toka could not lock its indexer daemon: {} is owned by another user",
            dir.display()
        ));
    }
    Ok(metadata)
}

/// FNV-1a 64-bit, naming one lock file per index: the name must be stable
/// for one database across processes and upgrades, distinct across
/// indexes, and short no matter how deep the database path runs.
#[cfg(unix)]
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The primary lock file for one index. The database identity is
/// normalized before hashing: `/data` and `/data/.` name one index and
/// must meet on one lock, or two supervisors would split after a reset
/// unlinks the legacy lock they shared.
#[cfg(unix)]
fn lock_file_name(database: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let identity = crate::owned_index::canonical_spelling(database);
    format!(
        "daemon-{:016x}.lock",
        fnv1a64(identity.as_os_str().as_bytes())
    )
}

#[cfg(unix)]
pub(crate) fn daemon_lock_path(database: &Path) -> PathBuf {
    daemon_runtime_dir().join(lock_file_name(database))
}

/// The beside-the-index lock file supervisors from before the runtime-dir
/// move take: new code co-takes it during the upgrade, and purge/reset
/// delete it as a known index file.
pub(crate) const LEGACY_DAEMON_LOCK_FILE_NAME: &str = "daemon.lock";

/// Tries the daemon lock without blocking: `Ok(None)` when another
/// supervisor already runs on this index, so the caller yields instead of
/// queueing behind it forever.
///
/// The primary lock lives in the runtime directory, deliberately outside
/// every path a reset deletes. A lock beside the index breaks silently
/// under `flock` + `unlink`: resetting while a supervisor holds the lock
/// unlinks the file out from under it, and the next starter locks a fresh
/// file — two supervisors, no error. Stopping before the reset (and
/// refusing to delete when the stop fails) closes the steady-state hole,
/// but not a supervisor that (re)starts mid-reset: a daemon revived by
/// `Restart=on-failure` after `kill -9`, by launchd `KeepAlive`, or by a
/// concurrent hand start locks a file the reset then unlinks, and the
/// starter after that locks another fresh one. A lock the reset never
/// unlinks has no such race: every contender meets on the same inode.
///
/// The beside-the-index `daemon.lock` is still taken too, so a supervisor
/// from before this move and one from after it cannot both run during the
/// upgrade: old code takes only the legacy lock, new code takes both, and
/// either side yields when the other holds it. Both locks drop together
/// with the guard.
///
/// Pre-delete checks (purge, reset) refuse while EITHER lock is held, so an
/// old hand-run supervisor blocks a reset instead of losing its lock file
/// mid-run. A residual upgrade-window race remains: an old binary started
/// after the check but before the deletion takes a legacy lock the
/// deletion then unlinks, and the next starter locks a fresh file — two
/// supervisors, no error. The window closes once old binaries stop
/// running; new binaries meet on the primary lock, which no reset unlinks.
#[cfg(unix)]
pub(crate) fn try_acquire_daemon_lock(database: &Path) -> Result<Option<DaemonLock>, String> {
    let path = daemon_lock_path(database);
    if let Some(runtime) = path.parent() {
        ensure_runtime_dir(runtime, current_uid())?;
    }
    let Some(primary) = lock_file(&path)? else {
        return Ok(None);
    };
    // The legacy lock's parent is ensured here, not just by the callers:
    // a reset may have wiped it while this supervisor was starting.
    crate::owned_index::ensure_index_parent(database)?;
    let Some(legacy) = lock_file(&database.with_file_name(LEGACY_DAEMON_LOCK_FILE_NAME))? else {
        return Ok(None);
    };
    Ok(Some(DaemonLock {
        _primary: primary,
        _legacy: legacy,
    }))
}

/// Whether another supervisor holds the daemon lock on `database` right
/// now: either the primary runtime lock or the legacy beside-the-index
/// one, since an old supervisor takes only the legacy lock. Purge and
/// reset refuse while either is held, so a live supervisor — managed or
/// hand-started — never watches its index vanish.
///
/// The probe creates nothing: only a supervisor that actually takes the
/// lock creates its file. A probe that created the legacy lock would plant
/// a `daemon.lock` that purge then deletes and reports, so a second purge
/// could never report nothing-to-do.
#[cfg(unix)]
pub(crate) fn daemon_lock_held(database: &Path) -> Result<bool, String> {
    let path = daemon_lock_path(database);
    if let Some(runtime) = path.parent() {
        ensure_runtime_dir(runtime, current_uid())?;
    }
    if lock_held_existing(&path)? {
        return Ok(true);
    }
    crate::owned_index::ensure_index_parent(database)?;
    lock_held_existing(&database.with_file_name(LEGACY_DAEMON_LOCK_FILE_NAME))
}

/// No-op lock where `flock` does not exist (see
/// [`try_acquire_daemon_lock`]): nothing can hold it.
#[cfg(not(unix))]
pub(crate) fn daemon_lock_held(_database: &Path) -> Result<bool, String> {
    Ok(false)
}

/// Whether `path`'s lock is held right now, without creating the file: a
/// missing lock file is never held.
#[cfg(unix)]
fn lock_held_existing(path: &Path) -> Result<bool, String> {
    let file = match fs::OpenOptions::new().write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!("Toka could not lock its indexer daemon: {error}"));
        }
    };
    flock_held(&file)
}

/// Whether `file` is locked by another supervisor: a non-blocking
/// exclusive `flock` that reports contention instead of waiting for it.
#[cfg(unix)]
fn flock_held(file: &fs::File) -> Result<bool, String> {
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(true);
        }
        return Err(format!("Toka could not lock its indexer daemon: {error}"));
    }
    Ok(false)
}

/// Opens and exclusively locks one lock file without blocking: `Ok(None)`
/// when another supervisor holds it.
#[cfg(unix)]
fn lock_file(path: &Path) -> Result<Option<fs::File>, String> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|error| format!("Toka could not lock its indexer daemon: {error}"))?;
    if flock_held(&file)? {
        return Ok(None);
    }
    Ok(Some(file))
}

/// Points the daemon lock at a per-process scratch dir, so lock tests
/// neither touch the real runtime dir nor contend with a live daemon. Set
/// once, never unset: every lock-acquiring test calls this first, and the
/// per-index file names keep tests apart within the shared dir.
#[cfg(all(test, unix))]
pub(crate) fn test_runtime_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("toka-test-runtime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("TOKA_RUNTIME_DIR", &dir);
        dir
    })
    .clone()
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
    // Folders currently awaiting generation, mirroring `pending`: the
    // heartbeat scopes the thumbnail count to online roots, which needs
    // the queued paths, not just their number.
    queued: Arc<Mutex<Vec<PathBuf>>>,
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
        let queued = Arc::new(Mutex::new(Vec::new()));
        let worker_queued = Arc::clone(&queued);
        std::thread::Builder::new()
            .name("toka-thumbnail-worker".into())
            .spawn(move || {
                while let Ok(folder) = receiver.recv() {
                    generator(&folder);
                    forget_queued(&worker_queued, &folder);
                    worker_pending.fetch_sub(1, Ordering::Relaxed);
                }
            })
            .expect("Toka could not start its thumbnail worker");
        Self {
            sender,
            pending,
            queued,
        }
    }

    pub(crate) fn enqueue(&self, folder: &Path) {
        // Reserve first: the worker's matching decrement runs only after
        // this folder is received, so the count can never dip below zero.
        self.pending.fetch_add(1, Ordering::Relaxed);
        self.queued.lock().unwrap().push(folder.to_path_buf());
        if self.sender.send(folder.to_path_buf()).is_err() {
            // The worker is gone; the folder will never generate.
            forget_queued(&self.queued, folder);
            self.pending.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Folders still waiting for (or undergoing) thumbnail generation.
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> u64 {
        self.pending.load(Ordering::Relaxed)
    }

    /// Folders awaiting generation outside `offline_roots`: queued work
    /// for missing storage waits with its root and never holds the
    /// queue, so the heartbeat counts it as `offline_roots` instead.
    pub(crate) fn pending_count_excluding(&self, offline_roots: &[PathBuf]) -> u64 {
        self.queued
            .lock()
            .unwrap()
            .iter()
            .filter(|folder| !offline_roots.iter().any(|root| folder.starts_with(root)))
            .count() as u64
    }
}

/// Drops one queued thumbnail folder after it generated (or when the
/// worker is gone and it never will), keeping the tracked queue beside
/// the atomic count.
fn forget_queued(queued: &Mutex<Vec<PathBuf>>, folder: &Path) {
    let mut queued = queued.lock().unwrap();
    if let Some(position) = queued.iter().position(|queued| queued == folder) {
        queued.swap_remove(position);
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
        // Offline roots are not actionable work in any count: the
        // thumbnail backlog is live, so the heartbeat scopes it here
        // while the database counts scope themselves. An unreadable roots
        // table scopes nothing — the queue query below fails the same way
        // and the beat is skipped.
        let offline_roots: Vec<PathBuf> = index
            .roots_with_state()
            .map(|roots| {
                roots
                    .into_iter()
                    .filter(|(_, _, state)| state == "offline")
                    .map(|(_, path, _)| path)
                    .collect()
            })
            .unwrap_or_default();
        let pending_thumbnails = self.thumbnails.pending_count_excluding(&offline_roots);
        let (activity, current_root, current_path) =
            self.current_activity(&index, pending_thumbnails);
        let queue = match index.daemon_queue_counts(pending_thumbnails) {
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
        pending_thumbnails: u64,
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
        if pending_thumbnails > 0 {
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

    /// A tree that scans slowly without ever committing a batch: one clip
    /// per top-level folder (100 files never fill the 128-file batch)
    /// under twenty thousand directories, so aborting mid-scan always
    /// strands seen-but-uncommitted files. Every top-level folder holds a
    /// clip, so whichever one the filesystem lists first, the second
    /// directory's savepoint already shows indexable work.
    fn write_slow_tree(root: &Path) {
        for top in 0..100 {
            let folder = root.join(format!("top-{top:02}"));
            std::fs::create_dir(&folder).unwrap();
            write_media(&folder, "clip.mp4");
            for leaf in 0..200 {
                std::fs::create_dir(folder.join(format!("leaf-{leaf:03}"))).unwrap();
            }
        }
    }

    /// Whether the root's latest scan run has classified files it has not
    /// committed yet: the mid-scan point a delete synchronizes with.
    fn scan_shows_indexable_work(database: &Path, root_id: &str) -> bool {
        OwnedIndex::open(database)
            .ok()
            .and_then(|index| index.scan_progress(root_id).ok().flatten())
            .is_some_and(|progress| progress.files_indexable > 0)
    }

    #[test]
    fn top_tags_are_due_when_never_counted_and_every_interval_after() {
        let counted = Instant::now();
        assert!(top_tags_due(None, counted));
        assert!(!top_tags_due(
            Some(counted),
            counted + Duration::from_secs(60)
        ));
        assert!(top_tags_due(Some(counted), counted + TOP_TAGS_INTERVAL));
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

    #[test]
    fn reconcile_starts_a_watcher_and_rows_appear() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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
    fn thumbnail_counts_exclude_folders_under_offline_roots() {
        let (release, release_receiver) = mpsc::channel::<()>();
        let thumbnails = ThumbnailWorker::with_generator(move |_| {
            let _ = release_receiver.recv();
        });
        thumbnails.enqueue(Path::new("/media/online"));
        thumbnails.enqueue(Path::new("/media/offline"));
        // Both folders count while the worker is blocked generating the
        // first; the offline one leaves the scoped count only.
        assert_eq!(thumbnails.pending_count(), 2);
        assert_eq!(
            thumbnails.pending_count_excluding(&[PathBuf::from("/media/offline")]),
            1
        );
        assert_eq!(thumbnails.pending_count_excluding(&[]), 2);

        drop(release);
        assert!(
            poll_until(Duration::from_secs(10), || thumbnails.pending_count() == 0),
            "expected the backlog to drain"
        );
        // The tracked queue drains beside the atomic count.
        assert_eq!(thumbnails.pending_count_excluding(&[]), 0);
    }

    #[test]
    fn heartbeat_publishes_liveness_and_follows_a_scan_to_idle() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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
        write_slow_tree(&media);
        let database = directory.path().join("state").join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        let id = manager.add_folder(&media).unwrap().folders[0].id.clone();

        let mut supervisor = test_supervisor(database.clone(), quiet_thumbnails());
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 1);
        // The storage vanishes mid-scan, never in a race with it: the scan
        // has demonstrably classified files it has not committed, with
        // thousands of directories still ahead of it. The mismatch is
        // recorded — not deleted — first, so the tree stays intact while
        // the pass stops the worker: renaming first would let a loaded
        // machine drain the missing directories (which fail fast, with no
        // savepoints) and finish the scan before the stop lands.
        assert!(
            poll_until(Duration::from_secs(30), || {
                scan_shows_indexable_work(&database, &id)
            }),
            "expected the scan to show indexable work before the delete"
        );
        OwnedIndex::open(&database)
            .unwrap()
            .record_mount(
                &id,
                Some(Path::new("/nonexistent-mount-target")),
                Some("nonexistent-source"),
                None,
            )
            .unwrap();
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 0);
        // The storage really is gone, too — after the offline marking, so
        // the removal's timing cannot race the scan.
        fs::rename(&media, directory.path().join("media-gone")).unwrap();
        supervisor.reconcile();
        assert_eq!(supervisor.active_count(), 0);

        // The aborted run really did strand work: still `scanning`, files
        // seen but none committed. The queue below excludes it anyway.
        let progress = OwnedIndex::open(&database)
            .unwrap()
            .scan_progress(&id)
            .unwrap()
            .unwrap();
        assert_eq!(progress.phase, ScanPhase::Scanning);
        assert!(progress.files_indexable > 0);
        assert_eq!(progress.files_indexed, 0);

        // Drain the thumbnail backlog before asserting an empty queue:
        // files committed before the abort enqueue generation the quiet
        // worker may not have collected yet, and the heartbeat would
        // count them. The bound guards a stuck worker, not a slow one.
        assert!(
            poll_until(Duration::from_secs(30), || supervisor
                .thumbnails
                .pending_count()
                == 0),
            "expected the thumbnail backlog to drain after the stop"
        );

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
    fn heartbeat_reports_waiting_when_a_completed_root_goes_offline() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("state").join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        manager.add_folder(&media).unwrap();

        // Thumbnails block, so the completed scan's queued folder is
        // still outstanding when the root goes offline: raw count 1,
        // deterministically.
        let (release, release_receiver) = mpsc::channel::<()>();
        let thumbnails = ThumbnailWorker::with_generator(move |_| {
            let _ = release_receiver.recv();
        });
        let mut supervisor = test_supervisor(database.clone(), thumbnails);
        supervisor.reconcile();
        assert!(
            poll_until(Duration::from_secs(10), || {
                supervisor
                    .watchers
                    .values()
                    .next()
                    .is_some_and(|entry| entry.watcher.live().stage == WorkerStage::Watching)
            }),
            "expected the scan to complete before the delete"
        );
        // The enqueue strictly precedes Watching, but the count crosses
        // threads on a relaxed atomic: poll for its arrival instead of
        // asserting a visibility lag cannot happen.
        assert!(
            poll_until(Duration::from_secs(10), || supervisor
                .thumbnails
                .pending_count()
                == 1),
            "expected the completed scan to queue one thumbnail folder"
        );

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
        assert_eq!(status.queue.pending_roots, 0);
        assert!(status.queue.is_empty());
        assert_eq!(status.offline_roots, 1);

        drop(release);
        assert!(
            poll_until(Duration::from_secs(10), || supervisor
                .thumbnails
                .pending_count()
                == 0),
            "expected the thumbnail backlog to drain"
        );
    }

    #[test]
    fn remount_and_rescan_returns_an_offline_root_to_idle() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_slow_tree(&media);
        let database = directory.path().join("state").join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        let id = manager.add_folder(&media).unwrap().folders[0].id.clone();

        let mut supervisor = test_supervisor(database.clone(), quiet_thumbnails());
        supervisor.reconcile();
        assert!(
            poll_until(Duration::from_secs(30), || {
                scan_shows_indexable_work(&database, &id)
            }),
            "expected the scan to show indexable work before the delete"
        );
        // Recorded — not deleted — first, as above: the tree stays intact
        // while the pass stops the worker mid-scan.
        OwnedIndex::open(&database)
            .unwrap()
            .record_mount(
                &id,
                Some(Path::new("/nonexistent-mount-target")),
                Some("nonexistent-source"),
                None,
            )
            .unwrap();
        supervisor.reconcile();
        fs::rename(&media, directory.path().join("media-gone")).unwrap();
        supervisor.reconcile();

        supervisor.publish_heartbeat();
        let status = crate::daemon_state::read_status(&database).unwrap();
        assert_eq!(
            status.activity,
            Some(crate::daemon_state::DaemonActivity::OfflineWaiting)
        );
        assert!(status.queue.is_empty());
        assert_eq!(status.offline_roots, 1);

        // The storage returns with a small tree on the same mount; the
        // rescan supersedes the aborted run and the daemon returns to idle
        // with nothing queued.
        fs::remove_dir_all(directory.path().join("media-gone")).unwrap();
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let current = mount_identity(&media);
        OwnedIndex::open(&database)
            .unwrap()
            .record_mount(
                &id,
                current.as_ref().map(|mount| mount.target.as_path()),
                current.as_ref().map(|mount| mount.source.as_str()),
                current.as_ref().and_then(|mount| mount.uuid.as_deref()),
            )
            .unwrap();
        // Gate on the rescan's first committed batch: Watching is also
        // the pre-start state, so polling for it alone can pass before
        // the worker runs a single file. The gate proves the scan
        // started; only then does Watching prove it finished.
        use std::sync::mpsc;
        let (gated_tx, gated_rx) = mpsc::channel::<()>();
        crate::owned_index::with_global_sync_probe(
            &id,
            move |site| {
                if site == crate::owned_index::SyncProbeSite::ScanBatchCommitted {
                    let _ = gated_tx.send(());
                }
            },
            || {
                supervisor.reconcile();
                assert_eq!(supervisor.active_count(), 1);
                gated_rx
                    .recv_timeout(Duration::from_secs(60))
                    .expect("the rescan must commit a first batch");
            },
        );
        // The gate proved the scan ran, so Watching now proves it
        // finished — the pre-start alias is gone.
        assert!(
            poll_until(Duration::from_secs(30), || {
                supervisor
                    .watchers
                    .values()
                    .next()
                    .is_some_and(|entry| entry.watcher.live().stage == WorkerStage::Watching)
            }),
            "expected the rescan to complete after the remount"
        );
        // The rescan queued thumbnails for an online root: let the quiet
        // worker drain them before asserting an empty queue.
        assert!(
            poll_until(Duration::from_secs(10), || supervisor
                .thumbnails
                .pending_count()
                == 0),
            "expected the thumbnail backlog to drain after the rescan"
        );

        supervisor.publish_heartbeat();
        let status = crate::daemon_state::read_status(&database).unwrap();
        assert_eq!(
            status.activity,
            Some(crate::daemon_state::DaemonActivity::Idle)
        );
        assert!(status.queue.is_empty());
        assert_eq!(status.offline_roots, 0);
    }

    #[test]
    fn heartbeat_reports_thumbnail_work_while_scans_are_idle() {
        let directory = tempdir().unwrap();
        let media = directory.path().join("media");
        fs::create_dir(&media).unwrap();
        write_media(&media, "clip.mp4");
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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
        let database = directory.path().join("state").join("index.sqlite3");
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

    #[cfg(unix)]
    #[test]
    fn fnv1a64_matches_the_reference_vectors() {
        assert_eq!(fnv1a64(b""), 0xcbf29ce484222325);
        assert_eq!(fnv1a64(b"foobar"), 0x85944171f73967e8);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_dir_prefers_the_override_then_the_platform_dir() {
        let dir = |path: &str| Some(PathBuf::from(path));
        // The override wins everywhere, exactly as given.
        assert_eq!(
            resolve_runtime_dir(
                dir("/scratch/runtime"),
                dir("/run/1"),
                dir("/c"),
                "/tmp".into(),
                1000
            ),
            PathBuf::from("/scratch/runtime")
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            resolve_runtime_dir(
                None,
                dir("/run/1000"),
                dir("/home/u/.cache"),
                "/tmp".into(),
                1000
            ),
            PathBuf::from("/run/1000/toka")
        );
        #[cfg(not(target_os = "linux"))]
        assert_eq!(
            resolve_runtime_dir(
                None,
                dir("/run/1000"),
                dir("/Users/u/Library/Caches"),
                "/tmp".into(),
                501
            ),
            PathBuf::from("/Users/u/Library/Caches/toka")
        );
        // No platform dir at all: the cache, then a per-user temporary dir.
        assert_eq!(
            resolve_runtime_dir(None, None, dir("/home/u/.cache"), "/tmp".into(), 1000),
            PathBuf::from("/home/u/.cache/toka")
        );
        assert_eq!(
            resolve_runtime_dir(None, None, None, "/tmp".into(), 1000),
            PathBuf::from("/tmp/toka-1000")
        );
        assert_eq!(
            resolve_runtime_dir(None, None, None, "/tmp".into(), 1001),
            PathBuf::from("/tmp/toka-1001")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_runtime_dir_is_pinned_to_its_user() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory = tempdir().unwrap();
        let uid = current_uid();

        // Created missing, and narrowed to 0700 even when it exists wider.
        // The secured result is a real directory owned by this user — read
        // back through `symlink_metadata`, never a following call.
        let fresh = directory.path().join("fresh");
        ensure_runtime_dir(&fresh, uid).unwrap();
        let secured = fs::symlink_metadata(&fresh).unwrap();
        assert!(!secured.file_type().is_symlink());
        assert_eq!(secured.uid(), uid);
        assert_eq!(secured.permissions().mode() & 0o777, 0o700);
        let wide = directory.path().join("wide");
        fs::create_dir(&wide).unwrap();
        fs::set_permissions(&wide, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_runtime_dir(&wide, uid).unwrap();
        assert_eq!(
            fs::metadata(&wide).unwrap().permissions().mode() & 0o777,
            0o700
        );

        // Another user's directory is refused, not joined: with a faked uid
        // this own-user directory reads as someone else's.
        let error = ensure_runtime_dir(&fresh, uid.wrapping_add(1)).unwrap_err();
        assert!(error.contains("owned by another user"), "{error}");

        // So is a symlink, which would redirect the locks elsewhere.
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&fresh, &link).unwrap();
        let error = ensure_runtime_dir(&link, uid).unwrap_err();
        assert!(error.contains("is a symlink"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn the_held_lock_check_sees_a_primary_supervisor() {
        test_runtime_dir();
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");

        assert!(
            !daemon_lock_held(&database).unwrap(),
            "no supervisor holds the lock yet"
        );
        let _daemon = try_acquire_daemon_lock(&database)
            .unwrap()
            .expect("test holds the daemon lock");
        assert!(daemon_lock_held(&database).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn the_held_lock_check_creates_no_lock_files() {
        // The probe must not plant the legacy lock it checks: purge would
        // then delete and report a file the purge itself created, and a
        // second purge could never report nothing-to-do.
        test_runtime_dir();
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");

        assert!(!daemon_lock_held(&database).unwrap());

        assert!(
            !database.with_file_name("daemon.lock").exists(),
            "probing must not create the legacy lock"
        );
        assert!(
            !daemon_lock_path(&database).exists(),
            "probing must not create the primary lock either"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_held_lock_check_sees_a_legacy_only_supervisor() {
        test_runtime_dir();
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        // An old supervisor, which takes only the beside-the-index lock.
        let legacy = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(database.with_file_name("daemon.lock"))
            .unwrap();
        use std::os::fd::AsRawFd;
        assert_eq!(
            unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        assert!(
            daemon_lock_held(&database).unwrap(),
            "a legacy-only lock must still block a purge or reset"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lock_paths_are_stable_per_index_and_distinct_across_indexes() {
        let runtime = test_runtime_dir();
        let first = runtime.join("a").join("index.sqlite3");
        let second = runtime.join("b").join("index.sqlite3");

        assert_eq!(daemon_lock_path(&first), daemon_lock_path(&first));
        assert_ne!(daemon_lock_path(&first), daemon_lock_path(&second));
        assert!(daemon_lock_path(&first).starts_with(&runtime));
    }

    #[cfg(unix)]
    #[test]
    fn aliased_database_spellings_share_one_lock() {
        test_runtime_dir();
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        let dotted = directory.path().join(".").join("index.sqlite3");

        assert_eq!(
            daemon_lock_path(&database),
            daemon_lock_path(&dotted),
            "spellings of one index must meet on one lock"
        );

        // And supervision actually meets there: a supervisor started
        // through one spelling blocks the other.
        let _daemon = try_acquire_daemon_lock(&database)
            .unwrap()
            .expect("test holds the daemon lock");
        assert!(
            try_acquire_daemon_lock(&dotted).unwrap().is_none(),
            "the aliased spelling must yield"
        );
    }

    #[cfg(unix)]
    #[test]
    fn acquiring_locks_the_runtime_file_and_the_legacy_file() {
        use std::os::fd::AsRawFd;
        test_runtime_dir();
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");

        let guard = try_acquire_daemon_lock(&database)
            .unwrap()
            .expect("first acquisition wins");

        // The primary lock lives outside the index folder, so a reset
        // never unlinks it; the legacy one stays beside the index for
        // supervisors from before the move.
        assert!(daemon_lock_path(&database).is_file());
        assert!(!daemon_lock_path(&database).starts_with(directory.path()));
        assert!(database.with_file_name("daemon.lock").is_file());
        assert!(
            try_acquire_daemon_lock(&database).unwrap().is_none(),
            "a second supervisor yields while the first holds the lock"
        );
        // Both files are really locked, not merely present: a fresh
        // open of either refuses a non-blocking exclusive lock.
        let _guard = guard;
        for path in [
            daemon_lock_path(&database),
            database.with_file_name("daemon.lock"),
        ] {
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            assert_eq!(held, -1, "{}", path.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_legacy_lock_held_by_an_old_supervisor_blocks_a_new_one() {
        test_runtime_dir();
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        // An old supervisor, which takes only the beside-the-index lock.
        let legacy = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(database.with_file_name("daemon.lock"))
            .unwrap();
        use std::os::fd::AsRawFd;
        assert_eq!(
            unsafe { libc::flock(legacy.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );

        assert!(
            try_acquire_daemon_lock(&database).unwrap().is_none(),
            "the new supervisor must yield to the legacy lock"
        );
    }

    #[cfg(unix)]
    #[test]
    fn two_indexes_gate_on_their_locks_concurrently_and_both_proceed() {
        // The daemon lock gates per index (and per user, through the
        // uid-scoped runtime dir): two supervisors gating concurrently on
        // different indexes both proceed — neither blocks the other.
        test_runtime_dir();
        let directory = tempdir().unwrap();
        let databases = [
            directory.path().join("a").join("index.sqlite3"),
            directory.path().join("b").join("index.sqlite3"),
        ];
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = databases
            .into_iter()
            .map(|database| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let guard = try_acquire_daemon_lock(&database).unwrap();
                    assert!(guard.is_some(), "each index gates on its own lock");
                    // Held past the other's acquisition, so the two locks
                    // really overlap.
                    barrier.wait();
                    drop(guard);
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
    }
}
