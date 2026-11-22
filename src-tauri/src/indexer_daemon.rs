//! The Linux indexer daemon: the systemd user service entry point, starting
//! and stopping the service, identifying mounts, logging, thumbnails, and
//! hosting the owned-index watchers.

use crate::owned_index::{OwnedIndex, OwnedIndexWatcher, ScanPhase};
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
    sync::{mpsc, Arc, Mutex},
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
const INDEXER_SERVICE: &str = "toka-indexer.service";

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
#[cfg(not(feature = "e2e"))]
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

#[cfg(feature = "e2e")]
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
        .map(|directory| directory.join("app.toka.desktop/logs/indexer.log"))
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

/// Runs the Linux indexer daemon: keeping one owned-index watcher per
/// configured root until the process stops. Each pass re-reads the roots
/// table, so folders added or removed while the window is closed apply on the
/// next wake-up.
pub fn run_daemon() -> Result<(), String> {
    let database = crate::owned_index::database_path()?;
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Toka could not create its index folder: {error}"))?;
    }
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
        let last_pass = Instant::now();
        drain_events(&receiver);
        match receiver.recv_timeout(RECONCILE_INTERVAL) {
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Toka's folder watcher stopped unexpectedly.".into())
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(_) => {
                drain_events(&receiver);
                std::thread::sleep(settle_delay(last_pass, Instant::now()));
            }
        }
    }
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
        std::thread::Builder::new()
            .name("toka-thumbnail-worker".into())
            .spawn(move || {
                while let Ok(folder) = receiver.recv() {
                    generator(&folder);
                }
            })
            .expect("Toka could not start its thumbnail worker");
        Self { sender }
    }

    pub(crate) fn enqueue(&self, folder: &Path) {
        let _ = self.sender.send(folder.to_path_buf());
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
}

/// How long the supervisor waits before restarting a watcher whose worker
/// exited, so a persistently failing scan cannot hot-loop the daemon.
const FAILED_SCAN_RETRY: Duration = Duration::from_secs(60);

/// Whether a watcher may (re)start now: always on a fresh root, only after
/// the cooldown following a worker exit.
fn retry_due(last_failure: Option<Instant>, now: Instant) -> bool {
    last_failure.is_none_or(|failure| now.duration_since(failure) >= FAILED_SCAN_RETRY)
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
            database,
            logger,
            thumbnails,
            watchers: HashMap::new(),
            last_failure: HashMap::new(),
        }
    }

    /// Starts watchers for newly configured roots and stops watchers whose
    /// root went away, reading the roots table fresh every time so a wake-up
    /// always picks up the latest configuration. Does nothing until the owned
    /// database exists, so it stays out of the way until folders are added.
    pub(crate) fn reconcile(&mut self) {
        if !self.database.is_file() {
            self.stop_all();
            return;
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
                    self.logger.record(format!(
                        "could not stop the search watcher for {}: {error}",
                        entry.path.display()
                    ));
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
                        self.logger.record(format!(
                            "could not stop the search watcher for {}: {error}",
                            path.display()
                        ));
                    }
                }
                if stored != "offline" && self.set_root_state(&id, "offline").is_err() {
                    break;
                }
                continue;
            }
            if stored == "offline" && self.set_root_state(&id, "pending").is_err() {
                break;
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
                if self
                    .record_mount(
                        &id,
                        current.as_ref().map(|mount| mount.target.as_path()),
                        current.as_ref().map(|mount| mount.source.as_str()),
                        current.as_ref().and_then(|mount| mount.uuid.as_deref()),
                    )
                    .is_err()
                {
                    break;
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
                self.logger.record(format!(
                    "could not stop the search watcher for {}: {error}",
                    entry.path.display()
                ));
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
