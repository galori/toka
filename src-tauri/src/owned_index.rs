use crate::{
    search::{is_supported_image, is_supported_video},
    tags,
};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(any(target_os = "macos", test))]
use std::sync::Mutex;

const SCHEMA_VERSION: i64 = 1;
const REPAIR_SCAN_INTERVAL: Duration = Duration::from_secs(15 * 60);

const SCHEMA: &str = r#"
CREATE TABLE schema_meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;

INSERT INTO schema_meta (key, value) VALUES
    ('schema_version', 1),
    ('index_revision', 0);

CREATE TABLE roots (
    id TEXT PRIMARY KEY,
    path BLOB NOT NULL UNIQUE,
    display_path TEXT NOT NULL,
    mount_target BLOB,
    mount_source TEXT,
    mount_uuid TEXT,
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'scanning', 'ready', 'offline', 'error')),
    scan_generation INTEGER NOT NULL DEFAULT 0,
    created_at_ns INTEGER,
    updated_at_ns INTEGER
);

CREATE TABLE files (
    id INTEGER PRIMARY KEY,
    path BLOB NOT NULL UNIQUE,
    display_path TEXT NOT NULL,
    file_name BLOB NOT NULL,
    extension TEXT NOT NULL,
    media_type TEXT NOT NULL CHECK (media_type IN ('video', 'image')),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    created_at_ns INTEGER,
    modified_at_ns INTEGER NOT NULL,
    file_name_search TEXT NOT NULL,
    tags_search TEXT NOT NULL,
    folders_search TEXT NOT NULL
);

CREATE INDEX files_extension ON files (extension);
CREATE INDEX files_media_type ON files (media_type);
CREATE INDEX files_size_bytes ON files (size_bytes);
CREATE INDEX files_created_at_ns ON files (created_at_ns);
CREATE INDEX files_modified_at_ns ON files (modified_at_ns);

CREATE TABLE root_files (
    root_id TEXT NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    scan_generation INTEGER NOT NULL,
    PRIMARY KEY (root_id, file_id)
) WITHOUT ROWID;

CREATE INDEX root_files_file_id ON root_files (file_id);

CREATE VIRTUAL TABLE search_docs USING fts5(
    file_name_search,
    tags_search,
    folders_search,
    content = 'files',
    content_rowid = 'id',
    tokenize = 'trigram remove_diacritics 1'
);

CREATE TRIGGER files_search_insert AFTER INSERT ON files BEGIN
    INSERT INTO search_docs (
        rowid, file_name_search, tags_search, folders_search
    ) VALUES (
        new.id, new.file_name_search, new.tags_search, new.folders_search
    );
END;

CREATE TRIGGER files_search_delete AFTER DELETE ON files BEGIN
    INSERT INTO search_docs (
        search_docs, rowid, file_name_search, tags_search, folders_search
    ) VALUES (
        'delete', old.id, old.file_name_search, old.tags_search, old.folders_search
    );
END;

CREATE TRIGGER files_search_update AFTER UPDATE OF
    file_name_search, tags_search, folders_search ON files BEGIN
    INSERT INTO search_docs (
        search_docs, rowid, file_name_search, tags_search, folders_search
    ) VALUES (
        'delete', old.id, old.file_name_search, old.tags_search, old.folders_search
    );
    INSERT INTO search_docs (
        rowid, file_name_search, tags_search, folders_search
    ) VALUES (
        new.id, new.file_name_search, new.tags_search, new.folders_search
    );
END;

CREATE TABLE scan_runs (
    id INTEGER PRIMARY KEY,
    root_id TEXT NOT NULL REFERENCES roots(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('scanning', 'ready', 'error')),
    folders_discovered INTEGER NOT NULL DEFAULT 0,
    folders_scanned INTEGER NOT NULL DEFAULT 0,
    files_discovered INTEGER NOT NULL DEFAULT 0,
    files_indexed INTEGER NOT NULL DEFAULT 0,
    videos_indexed INTEGER NOT NULL DEFAULT 0,
    images_indexed INTEGER NOT NULL DEFAULT 0,
    skipped_paths INTEGER NOT NULL DEFAULT 0,
    read_failures INTEGER NOT NULL DEFAULT 0,
    current_folder BLOB,
    current_file BLOB,
    started_at_ns INTEGER NOT NULL,
    updated_at_ns INTEGER NOT NULL,
    finished_at_ns INTEGER,
    error TEXT,
    UNIQUE (root_id, generation)
);

CREATE TABLE repair_queue (
    path BLOB PRIMARY KEY,
    reason TEXT NOT NULL,
    queued_at_ns INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT
) WITHOUT ROWID;
"#;

#[derive(Debug, thiserror::Error)]
pub(crate) enum OwnedIndexError {
    #[error("Toka could not use its search index: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("Toka cannot read search index schema version {0}.")]
    UnsupportedSchema(i64),
    #[error("That search folder is no longer configured.")]
    FolderNotConfigured,
    #[error("Toka could not secure {path}: {source}")]
    Permissions {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Toka could not read {path}: {source}")]
    PathRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Toka could not watch its search folders: {0}")]
    Watcher(#[from] notify::Error),
    #[error("Toka could not start its search watcher: {0}")]
    WorkerStart(#[source] std::io::Error),
    #[error("Toka's search watcher stopped unexpectedly.")]
    WorkerStopped,
    #[error("Toka could not fully scan {path}: {failures} path read failed.")]
    IncompleteScan { path: String, failures: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ScanPhase {
    Scanning,
    Ready,
    Error,
}

impl ScanPhase {
    fn from_database(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "scanning" => Ok(Self::Scanning),
            "ready" => Ok(Self::Ready),
            "error" => Ok(Self::Error),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }

    fn as_database(self) -> &'static str {
        match self {
            Self::Scanning => "scanning",
            Self::Ready => "ready",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScanProgress {
    pub root_id: String,
    pub phase: ScanPhase,
    pub folders_discovered: u64,
    pub folders_scanned: u64,
    pub files_discovered: u64,
    pub files_indexed: u64,
    pub videos_indexed: u64,
    pub images_indexed: u64,
    pub skipped_paths: u64,
    pub read_failures: u64,
    pub current_folder: Option<PathBuf>,
    pub current_file: Option<PathBuf>,
    pub started_at_ns: i64,
    pub updated_at_ns: i64,
    pub scan_rate: f64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingRepair {
    pub path: PathBuf,
    pub reason: String,
    pub attempts: u64,
    pub last_error: Option<String>,
}

impl ScanProgress {
    fn new(root_id: &str) -> Self {
        let now = now_ns();
        Self {
            root_id: root_id.to_owned(),
            phase: ScanPhase::Scanning,
            folders_discovered: 1,
            folders_scanned: 0,
            files_discovered: 0,
            files_indexed: 0,
            videos_indexed: 0,
            images_indexed: 0,
            skipped_paths: 0,
            read_failures: 0,
            current_folder: None,
            current_file: None,
            started_at_ns: now,
            updated_at_ns: now,
            scan_rate: 0.0,
            error: None,
        }
    }

    fn touch(&mut self) {
        self.updated_at_ns = now_ns();
        let elapsed_ns = self.updated_at_ns.saturating_sub(self.started_at_ns).max(1);
        self.scan_rate = self.files_indexed as f64 * 1_000_000_000.0 / elapsed_ns as f64;
    }
}

struct IndexedFile {
    path: PathBuf,
    path_bytes: Vec<u8>,
    file_name_bytes: Vec<u8>,
    extension: String,
    media_type: &'static str,
    size_bytes: i64,
    created_at_ns: Option<i64>,
    modified_at_ns: i64,
    file_name_search: String,
    tags_search: String,
    folders_search: String,
}

impl IndexedFile {
    fn read(path: PathBuf, metadata: fs::Metadata) -> Option<Self> {
        let media_type = if is_supported_video(&path) {
            "video"
        } else if is_supported_image(&path) {
            "image"
        } else {
            return None;
        };
        let size_bytes = i64::try_from(metadata.len()).ok()?;
        if size_bytes == 0 {
            return None;
        }
        let modified_at_ns = metadata.modified().ok().and_then(time_ns)?;
        let file_name = path.file_name()?.to_os_string();
        let display_name = file_name.to_string_lossy();
        let parsed_tags = tags::Tags::parse_file_name(&display_name);
        let file_name_search = tags::Tags::default()
            .apply_to_file(&display_name)
            .to_lowercase();
        let tags_search = parsed_tags.into_values().join(" ").to_lowercase();
        let folders_search = path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_string_lossy()
            .to_lowercase();

        Some(Self {
            path_bytes: path_bytes(&path),
            file_name_bytes: os_string_bytes(file_name),
            extension: path
                .extension()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase(),
            media_type,
            size_bytes,
            created_at_ns: metadata.created().ok().and_then(time_ns),
            modified_at_ns,
            file_name_search,
            tags_search,
            folders_search,
            path,
        })
    }
}

pub(crate) struct OwnedIndex {
    connection: Connection,
    path: PathBuf,
}

#[derive(Clone)]
pub(crate) struct OwnedIndexUpdates {
    database: Option<PathBuf>,
}

pub(crate) trait IndexUpdates: Send + Sync {
    fn synchronize_path(&self, path: &Path) -> Result<(), OwnedIndexError>;
    fn synchronize_rename(&self, from: &Path, to: &Path) -> Result<(), OwnedIndexError>;
}

impl OwnedIndexUpdates {
    pub(crate) fn new(database: PathBuf) -> Self {
        Self {
            database: Some(database),
        }
    }

    pub(crate) fn system() -> Self {
        Self {
            database: database_path().ok(),
        }
    }

    pub(crate) fn synchronize_path(&self, path: &Path) -> Result<(), OwnedIndexError> {
        let Some(database) = self.database.as_deref().filter(|path| path.is_file()) else {
            return Ok(());
        };
        let mut index = OwnedIndex::open(database)?;
        for (root_id, root) in index.roots()? {
            if path.starts_with(&root) {
                index.synchronize_path(&root_id, &root, path)?;
            }
        }
        Ok(())
    }

    pub(crate) fn synchronize_rename(&self, from: &Path, to: &Path) -> Result<(), OwnedIndexError> {
        let Some(database) = self.database.as_deref().filter(|path| path.is_file()) else {
            return Ok(());
        };
        let mut index = OwnedIndex::open(database)?;
        if let Some((root_id, root)) = index
            .roots()?
            .into_iter()
            .find(|(_, root)| from == root || (from.starts_with(root) && to.starts_with(root)))
        {
            index.synchronize_rename(&root_id, &root, from, to)?;
        }
        Ok(())
    }
}

impl IndexUpdates for OwnedIndexUpdates {
    fn synchronize_path(&self, path: &Path) -> Result<(), OwnedIndexError> {
        OwnedIndexUpdates::synchronize_path(self, path)
    }

    fn synchronize_rename(&self, from: &Path, to: &Path) -> Result<(), OwnedIndexError> {
        OwnedIndexUpdates::synchronize_rename(self, from, to)
    }
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum OwnedFolderStatus {
    Pending,
    Indexing,
    Ready,
    Offline,
    Error,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnedIndexFolder {
    pub id: String,
    pub path: PathBuf,
    pub status: OwnedFolderStatus,
    pub message: Option<String>,
    pub scanned_files: u64,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnedIndexState {
    pub supported: bool,
    pub revision: u64,
    pub indexed_videos: u64,
    pub indexed_images: u64,
    pub indexing_files: u64,
    pub folders: Vec<OwnedIndexFolder>,
}

#[cfg(any(target_os = "macos", test))]
pub(crate) struct OwnedIndexManager {
    database: PathBuf,
    watch: bool,
    watchers: Mutex<HashMap<String, ManagedRootWatcher>>,
}

#[cfg(any(target_os = "macos", test))]
struct ManagedRootWatcher {
    path: PathBuf,
    watcher: OwnedIndexWatcher,
}

#[cfg(any(target_os = "macos", test))]
impl OwnedIndexManager {
    pub(crate) fn system() -> Result<Self, String> {
        let manager = Self::new(database_path()?, true);
        manager.restore_watchers();
        Ok(manager)
    }

    #[cfg(test)]
    pub(crate) fn under(database: PathBuf) -> Self {
        Self::new(database, false)
    }

    fn new(database: PathBuf, watch: bool) -> Self {
        Self {
            database,
            watch,
            watchers: Mutex::new(HashMap::new()),
        }
    }

    fn restore_watchers(&self) {
        if let Err(error) = self.ensure_watchers() {
            eprintln!("Toka could not restore search folder watchers: {error}");
        }
    }

    pub(crate) fn state(&self) -> Result<OwnedIndexState, String> {
        self.ensure_watchers()?;
        let index = self.open().map_err(|error| error.to_string())?;
        let (indexed_videos, indexed_images) =
            index.media_counts().map_err(|error| error.to_string())?;
        let revision = index.revision().map_err(|error| error.to_string())?;
        let roots = index.roots().map_err(|error| error.to_string())?;
        let mut folders = Vec::new();
        let mut indexing_files = 0;

        for (id, path) in roots {
            let progress = index
                .scan_progress(&id)
                .map_err(|error| error.to_string())?;
            let status = if !path.is_dir() {
                OwnedFolderStatus::Offline
            } else {
                match progress.as_ref().map(|progress| progress.phase) {
                    Some(ScanPhase::Scanning) => OwnedFolderStatus::Indexing,
                    Some(ScanPhase::Ready) => OwnedFolderStatus::Ready,
                    Some(ScanPhase::Error) => OwnedFolderStatus::Error,
                    None => OwnedFolderStatus::Pending,
                }
            };
            let scanned_files = progress
                .as_ref()
                .map(|progress| progress.files_discovered)
                .unwrap_or(0);
            if status == OwnedFolderStatus::Indexing {
                indexing_files += scanned_files;
            }
            folders.push(OwnedIndexFolder {
                id,
                path,
                status,
                message: progress.and_then(|progress| progress.error),
                scanned_files,
            });
        }

        Ok(OwnedIndexState {
            supported: true,
            revision,
            indexed_videos,
            indexed_images,
            indexing_files,
            folders,
        })
    }

    pub(crate) fn add_folder(&self, requested: &Path) -> Result<OwnedIndexState, String> {
        let root = requested
            .canonicalize()
            .map_err(|error| format!("Toka could not open that folder: {error}"))?;
        if !root.is_dir() {
            return Err("Choose a folder to search.".into());
        }

        let mut watchers = self
            .watchers
            .lock()
            .map_err(|_| "Toka's search folder manager is unavailable.".to_owned())?;
        let id = {
            let index = self.open().map_err(|error| error.to_string())?;
            let existing = index.roots().map_err(|error| error.to_string())?;
            if existing
                .iter()
                .any(|(_, path)| root.starts_with(path) || path.starts_with(&root))
            {
                return Err("That folder is already covered by a Toka search folder.".into());
            }

            let id = uuid::Uuid::new_v4().to_string();
            let mut index = index;
            index
                .insert_root(&id, &root)
                .map_err(|error| error.to_string())?;
            id
        };

        if self.watch {
            match OwnedIndexWatcher::start(&self.database, id.clone(), root.clone()) {
                Ok(watcher) => {
                    watchers.insert(
                        id,
                        ManagedRootWatcher {
                            path: root,
                            watcher,
                        },
                    );
                }
                Err(error) => {
                    if let Ok(mut index) = self.open() {
                        let _ = index.remove_root(&id);
                    }
                    return Err(error.to_string());
                }
            }
        }

        drop(watchers);
        self.state()
    }

    pub(crate) fn remove_folder(&self, id: &str) -> Result<OwnedIndexState, String> {
        let mut watchers = self
            .watchers
            .lock()
            .map_err(|_| "Toka's search folder manager is unavailable.".to_owned())?;
        let watcher = watchers.remove(id);
        if let Some(watcher) = watcher {
            if let Err(error) = watcher.watcher.stop() {
                eprintln!("Toka could not stop a search folder watcher: {error}");
            }
        }
        {
            let mut index = self.open().map_err(|error| error.to_string())?;
            index.remove_root(id).map_err(|error| error.to_string())?;
        }
        drop(watchers);
        self.state()
    }

    fn open(&self) -> Result<OwnedIndex, OwnedIndexError> {
        OwnedIndex::open(&self.database)
    }

    fn ensure_watchers(&self) -> Result<(), String> {
        if !self.watch {
            return Ok(());
        }
        let mut watchers = self
            .watchers
            .lock()
            .map_err(|_| "Toka's search folder manager is unavailable.".to_owned())?;
        let index = self.open().map_err(|error| error.to_string())?;
        let roots = index.roots().map_err(|error| error.to_string())?;
        drop(index);
        let online_roots = roots
            .iter()
            .filter(|(_, root)| root.is_dir())
            .map(|(id, _)| id.as_str())
            .collect::<std::collections::HashSet<_>>();
        let stale_watchers = watchers
            .keys()
            .filter(|id| !online_roots.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        for id in stale_watchers {
            if let Some(watcher) = watchers.remove(&id) {
                if let Err(error) = watcher.watcher.stop() {
                    eprintln!("Toka could not stop a search folder watcher: {error}");
                }
            }
        }
        for (id, root) in roots {
            if !root.is_dir() {
                continue;
            }
            if watchers
                .get(&id)
                .is_some_and(|watcher| watcher.path == root)
            {
                continue;
            }
            if let Some(watcher) = watchers.remove(&id) {
                if let Err(error) = watcher.watcher.stop() {
                    eprintln!("Toka could not stop a search folder watcher: {error}");
                }
            }
            match OwnedIndexWatcher::start(&self.database, id.clone(), root.clone()) {
                Ok(watcher) => {
                    watchers.insert(
                        id,
                        ManagedRootWatcher {
                            path: root,
                            watcher,
                        },
                    );
                }
                Err(error) => eprintln!("Toka could not watch a search folder: {error}"),
            }
        }
        Ok(())
    }
}

pub(crate) fn database_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|directory| directory.join("toka").join("index.sqlite3"))
        .ok_or_else(|| "Toka could not find a place for its search index.".into())
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn root_paths(database: &Path) -> Result<Vec<PathBuf>, OwnedIndexError> {
    OwnedIndex::open(database)?
        .roots()
        .map(|roots| roots.into_iter().map(|(_, path)| path).collect())
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn index_revision(database: &Path) -> Result<u64, OwnedIndexError> {
    OwnedIndex::open(database)?.revision()
}

pub(crate) struct OwnedIndexWatcher {
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<Result<(), OwnedIndexError>>>,
}

impl OwnedIndexWatcher {
    pub(crate) fn start(
        database: &Path,
        root_id: String,
        root: PathBuf,
    ) -> Result<Self, OwnedIndexError> {
        Self::start_with_progress(database, root_id, root, |_| {})
    }

    pub(crate) fn start_with_progress<F>(
        database: &Path,
        root_id: String,
        root: PathBuf,
        on_progress: F,
    ) -> Result<Self, OwnedIndexError>
    where
        F: FnMut(&ScanProgress) + Send + 'static,
    {
        let index = OwnedIndex::open(database)?;
        let (event_sender, event_receiver) = mpsc::channel();
        let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
            let _ = event_sender.send(event);
        })?;
        watcher.watch(&root, RecursiveMode::Recursive)?;
        Self::spawn_with_events(index, root_id, root, event_receiver, watcher, on_progress)
    }

    #[cfg(test)]
    fn start_with_test_events<F>(
        database: &Path,
        root_id: String,
        root: PathBuf,
        mut on_progress: F,
    ) -> Result<(Self, mpsc::Sender<notify::Result<Event>>), OwnedIndexError>
    where
        F: FnMut(&ScanProgress, &mpsc::Sender<notify::Result<Event>>) + Send + 'static,
    {
        let index = OwnedIndex::open(database)?;
        let (event_sender, event_receiver) = mpsc::channel();
        let scan_sender = event_sender.clone();
        let watcher =
            Self::spawn_with_events(index, root_id, root, event_receiver, (), move |progress| {
                on_progress(progress, &scan_sender)
            })?;
        Ok((watcher, event_sender))
    }

    fn spawn_with_events<F, K>(
        mut index: OwnedIndex,
        root_id: String,
        root: PathBuf,
        event_receiver: mpsc::Receiver<notify::Result<Event>>,
        keepalive: K,
        mut on_progress: F,
    ) -> Result<Self, OwnedIndexError>
    where
        F: FnMut(&ScanProgress) + Send + 'static,
        K: Send + 'static,
    {
        let (stop, stop_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name(format!("toka-index-{root_id}"))
            .spawn(move || {
                let _keepalive = keepalive;
                index.scan_root(&root_id, &root, |progress| on_progress(progress))?;
                index.repair_pending(&root_id, &root)?;
                let mut event_state = NotifyEventState::default();
                let mut last_repair_scan = Instant::now();
                loop {
                    if stop_receiver.try_recv().is_ok() {
                        return Ok(());
                    }
                    match event_receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(Ok(event)) => {
                            if let Err(error) = apply_notify_event(
                                &mut index,
                                &root_id,
                                &root,
                                &mut event_state,
                                event,
                            ) {
                                eprintln!("Toka could not apply a search index event: {error}");
                            }
                        }
                        Ok(Err(error)) => {
                            eprintln!("Toka's folder watcher missed an event: {error}");
                            repair_root(&mut index, &root_id, &root);
                            last_repair_scan = Instant::now();
                        }
                        Err(mpsc::RecvTimeoutError::Timeout)
                            if last_repair_scan.elapsed() >= REPAIR_SCAN_INTERVAL =>
                        {
                            repair_root(&mut index, &root_id, &root);
                            last_repair_scan = Instant::now();
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            return Err(OwnedIndexError::WorkerStopped);
                        }
                    }
                }
            })
            .map_err(OwnedIndexError::WorkerStart)?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }

    pub(crate) fn stop(mut self) -> Result<(), OwnedIndexError> {
        let _ = self.stop.send(());
        match self.worker.take().expect("watcher worker missing").join() {
            Ok(result) => result,
            Err(_) => Err(OwnedIndexError::WorkerStopped),
        }
    }
}

impl Drop for OwnedIndexWatcher {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
struct NotifyEventState {
    renames: HashMap<usize, PathBuf>,
}

fn repair_root(index: &mut OwnedIndex, root_id: &str, root: &Path) {
    if let Err(error) = index.scan_root(root_id, root, |_| {}) {
        eprintln!("Toka could not repair its search index: {error}");
        return;
    }
    if let Err(error) = index.repair_pending(root_id, root) {
        eprintln!("Toka could not replay its search index repairs: {error}");
    }
}

fn apply_notify_event(
    index: &mut OwnedIndex,
    root_id: &str,
    root: &Path,
    state: &mut NotifyEventState,
    event: Event,
) -> Result<(), OwnedIndexError> {
    if event.need_rescan() {
        state.renames.clear();
        index.scan_root(root_id, root, |_| {})?;
        return Ok(());
    }
    match &event.kind {
        EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::Both)) => {
            if let (Some(from), Some(to)) = (event.paths.first(), event.paths.last()) {
                return index.synchronize_rename(root_id, root, from, to);
            }
        }
        EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::From)) => {
            if let (Some(tracker), Some(from)) = (event.tracker(), event.paths.first()) {
                state.renames.insert(tracker, from.clone());
                return Ok(());
            }
        }
        EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::To)) => {
            if let (Some(tracker), Some(to)) = (event.tracker(), event.paths.first()) {
                if let Some(from) = state.renames.remove(&tracker) {
                    return index.synchronize_rename(root_id, root, &from, to);
                }
            }
        }
        EventKind::Access(_) => return Ok(()),
        _ => {}
    }
    for path in event.paths {
        index.synchronize_path(root_id, root, &path)?;
    }
    Ok(())
}

impl OwnedIndex {
    pub(crate) fn open(path: &Path) -> Result<Self, OwnedIndexError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| OwnedIndexError::Permissions {
                path: parent.display().to_string(),
                source,
            })?;
            secure_directory(parent)?;
        }

        let mut connection = Connection::open(path)?;
        secure_file(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;\n\
             PRAGMA journal_mode = WAL;\n\
             PRAGMA synchronous = NORMAL;",
        )?;

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                let transaction = connection.transaction()?;
                transaction.execute_batch(SCHEMA)?;
                transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                transaction.commit()?;
            }
            SCHEMA_VERSION => {}
            other => return Err(OwnedIndexError::UnsupportedSchema(other)),
        }

        Ok(Self {
            connection,
            path: path.to_path_buf(),
        })
    }

    fn roots(&self) -> Result<Vec<(String, PathBuf)>, OwnedIndexError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, path FROM roots ORDER BY id")?;
        let rows =
            statement.query_map([], |row| Ok((row.get(0)?, path_from_bytes(row.get(1)?))))?;
        rows.collect::<Result<_, _>>()
            .map_err(OwnedIndexError::from)
    }

    fn roots_with_generations(&self) -> Result<Vec<(String, PathBuf, i64)>, OwnedIndexError> {
        let mut statement = self
            .connection
            .prepare("SELECT id, path, scan_generation FROM roots ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get(0)?, path_from_bytes(row.get(1)?), row.get(2)?))
        })?;
        rows.collect::<Result<_, _>>()
            .map_err(OwnedIndexError::from)
    }

    #[cfg(any(target_os = "macos", test))]
    fn insert_root(&mut self, id: &str, root: &Path) -> Result<(), OwnedIndexError> {
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO roots (
                id, path, display_path, state, created_at_ns, updated_at_ns
             ) VALUES (?1, ?2, ?3, 'pending', ?4, ?4)",
            params![id, path_bytes(root), root.to_string_lossy(), now_ns()],
        )?;
        transaction.execute(
            "UPDATE schema_meta SET value = value + 1 WHERE key = 'index_revision'",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(any(target_os = "macos", test))]
    fn remove_root(&mut self, id: &str) -> Result<(), OwnedIndexError> {
        let transaction = self.connection.transaction()?;
        let removed = transaction.execute("DELETE FROM roots WHERE id = ?1", [id])?;
        if removed == 0 {
            return Err(OwnedIndexError::FolderNotConfigured);
        }
        transaction.execute(
            "DELETE FROM files WHERE NOT EXISTS (
                SELECT 1 FROM root_files WHERE root_files.file_id = files.id
             )",
            [],
        )?;
        transaction.execute(
            "UPDATE schema_meta SET value = value + 1 WHERE key = 'index_revision'",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(any(target_os = "macos", test))]
    fn revision(&self) -> Result<u64, OwnedIndexError> {
        let revision: i64 = self.connection.query_row(
            "SELECT value FROM schema_meta WHERE key = 'index_revision'",
            [],
            |row| row.get(0),
        )?;
        Ok(revision.max(0) as u64)
    }

    #[cfg(any(target_os = "macos", test))]
    fn media_counts(&self) -> Result<(u64, u64), OwnedIndexError> {
        let (videos, images): (i64, i64) = self.connection.query_row(
            "SELECT
                COALESCE(SUM(media_type = 'video'), 0),
                COALESCE(SUM(media_type = 'image'), 0)
             FROM files",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((videos as u64, images as u64))
    }

    pub(crate) fn scan_root<F>(
        &mut self,
        root_id: &str,
        root: &Path,
        on_progress: F,
    ) -> Result<ScanProgress, OwnedIndexError>
    where
        F: FnMut(&ScanProgress),
    {
        self.scan_root_in_batches(root_id, root, 128, on_progress)
    }

    #[cfg(test)]
    fn scan_root_with_batch_size<F>(
        &mut self,
        root_id: &str,
        root: &Path,
        batch_size: usize,
        on_progress: F,
    ) -> Result<ScanProgress, OwnedIndexError>
    where
        F: FnMut(&ScanProgress),
    {
        self.scan_root_in_batches(root_id, root, batch_size.max(1), on_progress)
    }

    fn scan_root_in_batches<F>(
        &mut self,
        root_id: &str,
        root: &Path,
        batch_size: usize,
        mut on_progress: F,
    ) -> Result<ScanProgress, OwnedIndexError>
    where
        F: FnMut(&ScanProgress),
    {
        let (generation, mut progress) = self.begin_scan(root_id, root)?;
        on_progress(&progress);
        let mut pending = VecDeque::from([root.to_path_buf()]);
        let mut batch = Vec::with_capacity(batch_size);

        while let Some(directory) = pending.pop_front() {
            progress.current_folder = Some(directory.clone());
            progress.current_file = None;
            progress.touch();
            on_progress(&progress);

            let entries = match fs::read_dir(&directory) {
                Ok(entries) => entries,
                Err(_) => {
                    progress.read_failures = progress.read_failures.saturating_add(1);
                    progress.touch();
                    on_progress(&progress);
                    continue;
                }
            };
            progress.folders_scanned = progress.folders_scanned.saturating_add(1);

            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        progress.read_failures = progress.read_failures.saturating_add(1);
                        progress.touch();
                        on_progress(&progress);
                        continue;
                    }
                };
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(_) => {
                        progress.read_failures = progress.read_failures.saturating_add(1);
                        progress.current_file = Some(path);
                        progress.touch();
                        on_progress(&progress);
                        continue;
                    }
                };
                if file_type.is_dir() {
                    progress.folders_discovered = progress.folders_discovered.saturating_add(1);
                    pending.push_back(path);
                    continue;
                }
                if !file_type.is_file() {
                    progress.skipped_paths = progress.skipped_paths.saturating_add(1);
                    progress.current_file = Some(path);
                    progress.touch();
                    on_progress(&progress);
                    continue;
                }

                progress.files_discovered = progress.files_discovered.saturating_add(1);
                progress.current_file = Some(path.clone());
                progress.touch();
                on_progress(&progress);
                if self.is_own_index_path(&path)
                    || (!is_supported_video(&path) && !is_supported_image(&path))
                {
                    continue;
                }
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(_) => {
                        progress.read_failures = progress.read_failures.saturating_add(1);
                        progress.touch();
                        on_progress(&progress);
                        continue;
                    }
                };
                if metadata.len() == 0 {
                    progress.skipped_paths = progress.skipped_paths.saturating_add(1);
                    progress.touch();
                    on_progress(&progress);
                    continue;
                }
                let Some(file) = IndexedFile::read(path, metadata) else {
                    progress.read_failures = progress.read_failures.saturating_add(1);
                    progress.touch();
                    on_progress(&progress);
                    continue;
                };
                batch.push(file);
                if batch.len() >= batch_size {
                    self.commit_batch(root_id, generation, &mut batch, &mut progress)?;
                    on_progress(&progress);
                }
            }
        }

        if !batch.is_empty() {
            self.commit_batch(root_id, generation, &mut batch, &mut progress)?;
            on_progress(&progress);
        }

        if progress.read_failures > 0 {
            let error = OwnedIndexError::IncompleteScan {
                path: root.display().to_string(),
                failures: progress.read_failures,
            };
            self.finish_failed_scan(&mut progress, &error.to_string())?;
            on_progress(&progress);
            return Err(error);
        }

        self.finish_successful_scan(generation, &mut progress)?;
        on_progress(&progress);
        Ok(progress)
    }

    fn begin_scan(
        &mut self,
        root_id: &str,
        root: &Path,
    ) -> Result<(i64, ScanProgress), OwnedIndexError> {
        let progress = ScanProgress::new(root_id);
        let root_bytes = path_bytes(root);
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO roots (
                id, path, display_path, state, scan_generation,
                created_at_ns, updated_at_ns
             ) VALUES (?1, ?2, ?3, 'scanning', 1, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET
                path = excluded.path,
                display_path = excluded.display_path,
                state = 'scanning',
                scan_generation = roots.scan_generation + 1,
                updated_at_ns = excluded.updated_at_ns",
            params![
                root_id,
                root_bytes,
                root.to_string_lossy(),
                progress.started_at_ns
            ],
        )?;
        let generation = transaction.query_row(
            "SELECT scan_generation FROM roots WHERE id = ?1",
            [root_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO scan_runs (
                root_id, generation, state, folders_discovered,
                started_at_ns, updated_at_ns
             ) VALUES (?1, ?2, 'scanning', 1, ?3, ?3)",
            params![root_id, generation, progress.started_at_ns],
        )?;
        transaction.commit()?;
        Ok((generation, progress))
    }

    fn commit_batch(
        &mut self,
        root_id: &str,
        generation: i64,
        batch: &mut Vec<IndexedFile>,
        progress: &mut ScanProgress,
    ) -> Result<(), OwnedIndexError> {
        let transaction = self.connection.transaction()?;
        let mut videos = 0u64;
        let mut images = 0u64;
        for file in batch.drain(..) {
            transaction.execute(
                "INSERT INTO files (
                    path, display_path, file_name, extension, media_type,
                    size_bytes, created_at_ns, modified_at_ns,
                    file_name_search, tags_search, folders_search
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(path) DO UPDATE SET
                    display_path = excluded.display_path,
                    file_name = excluded.file_name,
                    extension = excluded.extension,
                    media_type = excluded.media_type,
                    size_bytes = excluded.size_bytes,
                    created_at_ns = excluded.created_at_ns,
                    modified_at_ns = excluded.modified_at_ns,
                    file_name_search = excluded.file_name_search,
                    tags_search = excluded.tags_search,
                    folders_search = excluded.folders_search",
                params![
                    file.path_bytes,
                    file.path.to_string_lossy(),
                    file.file_name_bytes,
                    file.extension,
                    file.media_type,
                    file.size_bytes,
                    file.created_at_ns,
                    file.modified_at_ns,
                    file.file_name_search,
                    file.tags_search,
                    file.folders_search,
                ],
            )?;
            let file_id: i64 = transaction.query_row(
                "SELECT id FROM files WHERE path = ?1",
                [file.path_bytes],
                |row| row.get(0),
            )?;
            transaction.execute(
                "INSERT INTO root_files (root_id, file_id, scan_generation)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(root_id, file_id) DO UPDATE SET
                    scan_generation = excluded.scan_generation",
                params![root_id, file_id, generation],
            )?;
            if file.media_type == "video" {
                videos = videos.saturating_add(1);
            } else {
                images = images.saturating_add(1);
            }
        }
        progress.files_indexed = progress
            .files_indexed
            .saturating_add(videos.saturating_add(images));
        progress.videos_indexed = progress.videos_indexed.saturating_add(videos);
        progress.images_indexed = progress.images_indexed.saturating_add(images);
        progress.touch();
        persist_progress(&transaction, progress)?;
        transaction.execute(
            "UPDATE schema_meta SET value = value + 1 WHERE key = 'index_revision'",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn finish_failed_scan(
        &mut self,
        progress: &mut ScanProgress,
        message: &str,
    ) -> Result<(), OwnedIndexError> {
        progress.phase = ScanPhase::Error;
        progress.current_folder = None;
        progress.current_file = None;
        progress.error = Some(message.to_owned());
        progress.touch();
        let transaction = self.connection.transaction()?;
        persist_progress(&transaction, progress)?;
        transaction.execute(
            "UPDATE roots SET state = 'error', updated_at_ns = ?2 WHERE id = ?1",
            params![progress.root_id, progress.updated_at_ns],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn finish_successful_scan(
        &mut self,
        generation: i64,
        progress: &mut ScanProgress,
    ) -> Result<(), OwnedIndexError> {
        progress.phase = ScanPhase::Ready;
        progress.current_folder = None;
        progress.current_file = None;
        progress.error = None;
        progress.touch();
        let transaction = self.connection.transaction()?;
        transaction.execute(
            "DELETE FROM root_files
             WHERE root_id = ?1 AND scan_generation != ?2",
            params![progress.root_id, generation],
        )?;
        transaction.execute(
            "DELETE FROM files
             WHERE NOT EXISTS (
                SELECT 1 FROM root_files WHERE root_files.file_id = files.id
             )",
            [],
        )?;
        persist_progress(&transaction, progress)?;
        transaction.execute(
            "UPDATE roots SET state = 'ready', updated_at_ns = ?2 WHERE id = ?1",
            params![progress.root_id, progress.updated_at_ns],
        )?;
        transaction.execute(
            "UPDATE schema_meta SET value = value + 1 WHERE key = 'index_revision'",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn scan_progress(
        &self,
        root_id: &str,
    ) -> Result<Option<ScanProgress>, OwnedIndexError> {
        self.connection
            .query_row(
                "SELECT root_id, state,
                    folders_discovered, folders_scanned,
                    files_discovered, files_indexed,
                    videos_indexed, images_indexed,
                    skipped_paths, read_failures,
                    current_folder, current_file,
                    started_at_ns, updated_at_ns, error
                 FROM scan_runs
                 WHERE root_id = ?1
                 ORDER BY generation DESC
                 LIMIT 1",
                [root_id],
                |row| {
                    let phase = ScanPhase::from_database(&row.get::<_, String>(1)?)?;
                    let started_at_ns: i64 = row.get(12)?;
                    let updated_at_ns: i64 = row.get(13)?;
                    let files_indexed = row.get::<_, i64>(5)? as u64;
                    let elapsed_ns = updated_at_ns.saturating_sub(started_at_ns).max(1);
                    Ok(ScanProgress {
                        root_id: row.get(0)?,
                        phase,
                        folders_discovered: row.get::<_, i64>(2)? as u64,
                        folders_scanned: row.get::<_, i64>(3)? as u64,
                        files_discovered: row.get::<_, i64>(4)? as u64,
                        files_indexed,
                        videos_indexed: row.get::<_, i64>(6)? as u64,
                        images_indexed: row.get::<_, i64>(7)? as u64,
                        skipped_paths: row.get::<_, i64>(8)? as u64,
                        read_failures: row.get::<_, i64>(9)? as u64,
                        current_folder: row.get::<_, Option<Vec<u8>>>(10)?.map(path_from_bytes),
                        current_file: row.get::<_, Option<Vec<u8>>>(11)?.map(path_from_bytes),
                        started_at_ns,
                        updated_at_ns,
                        scan_rate: files_indexed as f64 * 1_000_000_000.0 / elapsed_ns as f64,
                        error: row.get(14)?,
                    })
                },
            )
            .optional()
            .map_err(OwnedIndexError::from)
    }

    pub(crate) fn synchronize_path(
        &mut self,
        root_id: &str,
        root: &Path,
        path: &Path,
    ) -> Result<(), OwnedIndexError> {
        match self.try_synchronize_path(root_id, root, path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = error.to_string();
                self.queue_repair(path, "direct update", &message)?;
                Err(error)
            }
        }
    }

    fn try_synchronize_path(
        &mut self,
        root_id: &str,
        root: &Path,
        path: &Path,
    ) -> Result<(), OwnedIndexError> {
        if !path.starts_with(root) {
            return Ok(());
        }
        let metadata = if self.is_own_index_path(path) {
            None
        } else {
            match fs::metadata(path) {
                Ok(metadata) => Some(metadata),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => {
                    return Err(OwnedIndexError::PathRead {
                        path: path.display().to_string(),
                        source,
                    })
                }
            }
        };
        if metadata.as_ref().is_some_and(fs::Metadata::is_dir) {
            return self.synchronize_tree(root_id, root, path);
        }
        let Some(file) = metadata
            .filter(fs::Metadata::is_file)
            .and_then(|metadata| IndexedFile::read(path.to_path_buf(), metadata))
        else {
            return self.remove_indexed_path(root_id, path);
        };
        let transaction = self.connection.transaction()?;
        let generation: i64 = transaction.query_row(
            "SELECT scan_generation FROM roots WHERE id = ?1",
            [root_id],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO files (
                path, display_path, file_name, extension, media_type,
                size_bytes, created_at_ns, modified_at_ns,
                file_name_search, tags_search, folders_search
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(path) DO UPDATE SET
                display_path = excluded.display_path,
                file_name = excluded.file_name,
                extension = excluded.extension,
                media_type = excluded.media_type,
                size_bytes = excluded.size_bytes,
                created_at_ns = excluded.created_at_ns,
                modified_at_ns = excluded.modified_at_ns,
                file_name_search = excluded.file_name_search,
                tags_search = excluded.tags_search,
                folders_search = excluded.folders_search",
            params![
                file.path_bytes,
                file.path.to_string_lossy(),
                file.file_name_bytes,
                file.extension,
                file.media_type,
                file.size_bytes,
                file.created_at_ns,
                file.modified_at_ns,
                file.file_name_search,
                file.tags_search,
                file.folders_search,
            ],
        )?;
        let file_id: i64 = transaction.query_row(
            "SELECT id FROM files WHERE path = ?1",
            [file.path_bytes],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO root_files (root_id, file_id, scan_generation)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(root_id, file_id) DO UPDATE SET
                scan_generation = excluded.scan_generation",
            params![root_id, file_id, generation],
        )?;
        transaction.execute(
            "UPDATE schema_meta SET value = value + 1 WHERE key = 'index_revision'",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn synchronize_tree(
        &mut self,
        root_id: &str,
        root: &Path,
        tree: &Path,
    ) -> Result<(), OwnedIndexError> {
        let mut pending = VecDeque::from([tree.to_path_buf()]);
        while let Some(directory) = pending.pop_front() {
            let entries = fs::read_dir(&directory).map_err(|source| OwnedIndexError::PathRead {
                path: directory.display().to_string(),
                source,
            })?;
            for entry in entries {
                let entry = entry.map_err(|source| OwnedIndexError::PathRead {
                    path: directory.display().to_string(),
                    source,
                })?;
                let path = entry.path();
                let file_type = entry
                    .file_type()
                    .map_err(|source| OwnedIndexError::PathRead {
                        path: path.display().to_string(),
                        source,
                    })?;
                if file_type.is_dir() {
                    pending.push_back(path);
                } else if file_type.is_file() {
                    self.try_synchronize_path(root_id, root, &path)?;
                }
            }
        }
        Ok(())
    }

    fn remove_indexed_path(&mut self, root_id: &str, path: &Path) -> Result<(), OwnedIndexError> {
        let file_ids = {
            let mut statement = self.connection.prepare(
                "SELECT files.id, files.path
                 FROM files
                 JOIN root_files ON root_files.file_id = files.id
                 WHERE root_files.root_id = ?1",
            )?;
            let rows = statement.query_map([root_id], |row| {
                Ok((row.get::<_, i64>(0)?, path_from_bytes(row.get(1)?)))
            })?;
            let mut file_ids = Vec::new();
            for row in rows {
                let (file_id, indexed_path) = row?;
                if indexed_path.starts_with(path) {
                    file_ids.push(file_id);
                }
            }
            file_ids
        };
        let transaction = self.connection.transaction()?;
        for file_id in file_ids {
            transaction.execute(
                "DELETE FROM root_files WHERE root_id = ?1 AND file_id = ?2",
                params![root_id, file_id],
            )?;
        }
        transaction.execute(
            "DELETE FROM files
             WHERE NOT EXISTS (
                SELECT 1 FROM root_files WHERE root_files.file_id = files.id
             )",
            [],
        )?;
        transaction.execute(
            "UPDATE schema_meta SET value = value + 1 WHERE key = 'index_revision'",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn queue_repair(
        &mut self,
        path: &Path,
        reason: &str,
        last_error: &str,
    ) -> Result<(), OwnedIndexError> {
        self.connection.execute(
            "INSERT INTO repair_queue (
                path, reason, queued_at_ns, attempts, last_error
             ) VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(path) DO UPDATE SET
                reason = excluded.reason,
                queued_at_ns = excluded.queued_at_ns,
                attempts = repair_queue.attempts + 1,
                last_error = excluded.last_error",
            params![path_bytes(path), reason, now_ns(), last_error],
        )?;
        Ok(())
    }

    pub(crate) fn pending_repairs(&self) -> Result<Vec<PendingRepair>, OwnedIndexError> {
        let mut statement = self.connection.prepare(
            "SELECT path, reason, attempts, last_error
             FROM repair_queue
             ORDER BY queued_at_ns, path",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(PendingRepair {
                path: path_from_bytes(row.get(0)?),
                reason: row.get(1)?,
                attempts: row.get::<_, i64>(2)? as u64,
                last_error: row.get(3)?,
            })
        })?;
        rows.collect::<Result<_, _>>()
            .map_err(OwnedIndexError::from)
    }

    pub(crate) fn repair_pending(
        &mut self,
        root_id: &str,
        root: &Path,
    ) -> Result<usize, OwnedIndexError> {
        let repairs = self.pending_repairs()?;
        let mut repaired = 0;
        for repair in repairs
            .into_iter()
            .filter(|repair| repair.path.starts_with(root))
        {
            let result = if repair.reason == "direct rename" {
                self.scan_root(root_id, root, |_| {}).map(|_| ())
            } else {
                self.try_synchronize_path(root_id, root, &repair.path)
            };
            match result {
                Ok(()) => {
                    self.connection.execute(
                        "DELETE FROM repair_queue WHERE path = ?1",
                        [path_bytes(&repair.path)],
                    )?;
                    repaired += 1;
                }
                Err(error) => {
                    self.queue_repair(&repair.path, &repair.reason, &error.to_string())?;
                }
            }
        }
        Ok(repaired)
    }

    pub(crate) fn synchronize_rename(
        &mut self,
        root_id: &str,
        root: &Path,
        from: &Path,
        to: &Path,
    ) -> Result<(), OwnedIndexError> {
        match self.try_synchronize_rename(root_id, root, from, to) {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = error.to_string();
                self.queue_repair(to, "direct rename", &message)?;
                Err(error)
            }
        }
    }

    fn try_synchronize_rename(
        &mut self,
        root_id: &str,
        root: &Path,
        from: &Path,
        to: &Path,
    ) -> Result<(), OwnedIndexError> {
        let root_moved = from == root;
        if !root_moved {
            match (from.starts_with(root), to.starts_with(root)) {
                (false, false) => return Ok(()),
                (true, false) => return self.remove_indexed_path(root_id, from),
                (false, true) => return self.try_synchronize_path(root_id, root, to),
                (true, true) => {}
            }
        }
        let mut root_memberships = self.roots_with_generations()?;
        if root_moved {
            for (candidate_id, candidate_root, _) in &mut root_memberships {
                if candidate_id == root_id {
                    *candidate_root = to.to_path_buf();
                }
            }
        }
        let moved_files = {
            let mut statement = self.connection.prepare("SELECT id, path FROM files")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, path_from_bytes(row.get(1)?)))
            })?;
            let mut moved_files = Vec::new();
            for row in rows {
                let (file_id, old_path) = row?;
                let Ok(relative) = old_path.strip_prefix(from) else {
                    continue;
                };
                let new_path = if relative.as_os_str().is_empty() {
                    to.to_path_buf()
                } else {
                    to.join(relative)
                };
                let Some(file) = fs::metadata(&new_path)
                    .ok()
                    .filter(|metadata| metadata.is_file())
                    .and_then(|metadata| IndexedFile::read(new_path, metadata))
                else {
                    continue;
                };
                moved_files.push((file_id, file));
            }
            moved_files
        };
        let transaction = self.connection.transaction()?;
        transaction.query_row(
            "SELECT scan_generation FROM roots WHERE id = ?1",
            [root_id],
            |row| row.get::<_, i64>(0),
        )?;
        if root_moved {
            transaction.execute(
                "UPDATE roots
                 SET path = ?2, display_path = ?3, updated_at_ns = ?4
                 WHERE id = ?1",
                params![root_id, path_bytes(to), to.to_string_lossy(), now_ns()],
            )?;
        }
        for (file_id, file) in moved_files {
            transaction.execute(
                "UPDATE files SET
                    path = ?2,
                    display_path = ?3,
                    file_name = ?4,
                    extension = ?5,
                    media_type = ?6,
                    size_bytes = ?7,
                    created_at_ns = ?8,
                    modified_at_ns = ?9,
                    file_name_search = ?10,
                    tags_search = ?11,
                    folders_search = ?12
                 WHERE id = ?1",
                params![
                    file_id,
                    file.path_bytes,
                    file.path.to_string_lossy(),
                    file.file_name_bytes,
                    file.extension,
                    file.media_type,
                    file.size_bytes,
                    file.created_at_ns,
                    file.modified_at_ns,
                    file.file_name_search,
                    file.tags_search,
                    file.folders_search,
                ],
            )?;
            for (candidate_id, candidate_root, generation) in &root_memberships {
                if file.path.starts_with(candidate_root) {
                    transaction.execute(
                        "INSERT INTO root_files (root_id, file_id, scan_generation)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(root_id, file_id) DO UPDATE SET
                            scan_generation = excluded.scan_generation",
                        params![candidate_id, file_id, generation],
                    )?;
                } else {
                    transaction.execute(
                        "DELETE FROM root_files WHERE root_id = ?1 AND file_id = ?2",
                        params![candidate_id, file_id],
                    )?;
                }
            }
        }
        transaction.execute(
            "UPDATE schema_meta SET value = value + 1 WHERE key = 'index_revision'",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn is_own_index_path(&self, path: &Path) -> bool {
        if path == self.path {
            return true;
        }
        ["-wal", "-shm"]
            .into_iter()
            .map(|suffix| {
                let mut value = self.path.as_os_str().to_os_string();
                value.push(suffix);
                PathBuf::from(value)
            })
            .any(|owned| path == owned)
    }
}

fn persist_progress(
    transaction: &Transaction<'_>,
    progress: &ScanProgress,
) -> Result<(), rusqlite::Error> {
    let current_folder = progress.current_folder.as_deref().map(path_bytes);
    let current_file = progress.current_file.as_deref().map(path_bytes);
    let finished_at_ns = (progress.phase != ScanPhase::Scanning).then_some(progress.updated_at_ns);
    transaction.execute(
        "UPDATE scan_runs SET
            state = ?2,
            folders_discovered = ?3,
            folders_scanned = ?4,
            files_discovered = ?5,
            files_indexed = ?6,
            videos_indexed = ?7,
            images_indexed = ?8,
            skipped_paths = ?9,
            read_failures = ?10,
            current_folder = ?11,
            current_file = ?12,
            updated_at_ns = ?13,
            finished_at_ns = ?14,
            error = ?15
         WHERE root_id = ?1
           AND generation = (
                SELECT scan_generation FROM roots WHERE id = ?1
           )",
        params![
            progress.root_id,
            progress.phase.as_database(),
            database_count(progress.folders_discovered),
            database_count(progress.folders_scanned),
            database_count(progress.files_discovered),
            database_count(progress.files_indexed),
            database_count(progress.videos_indexed),
            database_count(progress.images_indexed),
            database_count(progress.skipped_paths),
            database_count(progress.read_failures),
            current_folder,
            current_file,
            progress.updated_at_ns,
            finished_at_ns,
            progress.error,
        ],
    )?;
    Ok(())
}

fn database_count(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn now_ns() -> i64 {
    time_ns(SystemTime::now()).unwrap_or_default()
}

fn time_ns(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).ok(),
        Err(error) => i64::try_from(error.duration().as_nanos())
            .ok()
            .and_then(i64::checked_neg),
    }
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn os_string_bytes(value: OsString) -> Vec<u8> {
    use std::os::unix::ffi::OsStringExt;

    value.into_vec()
}

#[cfg(not(unix))]
fn os_string_bytes(value: OsString) -> Vec<u8> {
    value.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn path_from_bytes(value: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;

    PathBuf::from(OsString::from_vec(value))
}

#[cfg(not(unix))]
fn path_from_bytes(value: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&value).into_owned())
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<(), OwnedIndexError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
        OwnedIndexError::Permissions {
            path: path.display().to_string(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn secure_directory(_path: &Path) -> Result<(), OwnedIndexError> {
    Ok(())
}

#[cfg(unix)]
fn secure_file(path: &Path) -> Result<(), OwnedIndexError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
        OwnedIndexError::Permissions {
            path: path.display().to_string(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn secure_file(_path: &Path) -> Result<(), OwnedIndexError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use tempfile::tempdir;

    fn open_index() -> (tempfile::TempDir, OwnedIndex) {
        let directory = tempdir().unwrap();
        let index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        (directory, index)
    }

    #[test]
    fn opening_an_index_creates_the_versioned_wal_schema() {
        let (_directory, index) = open_index();
        let journal_mode: String = index
            .connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let foreign_keys: i64 = index
            .connection
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        let version: i64 = index
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let tables = index
            .connection
            .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'view')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<BTreeSet<_>, _>>()
            .unwrap();

        assert_eq!(journal_mode, "wal");
        assert_eq!(foreign_keys, 1);
        assert_eq!(version, 1);
        for table in [
            "schema_meta",
            "roots",
            "files",
            "root_files",
            "search_docs",
            "scan_runs",
            "repair_queue",
        ] {
            assert!(tables.contains(table), "missing table {table}");
        }
    }

    #[test]
    fn overlapping_roots_share_one_file_record() {
        let (_directory, index) = open_index();
        index
            .connection
            .execute(
                "INSERT INTO roots (id, path, display_path) VALUES (?1, ?2, ?3)",
                ("child", b"/media/photos".as_slice(), "/media/photos"),
            )
            .unwrap();
        index
            .connection
            .execute(
                "INSERT INTO roots (id, path, display_path) VALUES (?1, ?2, ?3)",
                ("parent", b"/media".as_slice(), "/media"),
            )
            .unwrap();
        index
            .connection
            .execute(
                "INSERT INTO files (
                    path, display_path, file_name, extension, media_type,
                    size_bytes, created_at_ns, modified_at_ns,
                    file_name_search, tags_search, folders_search
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    b"/media/photos/trip.jpg".as_slice(),
                    "/media/photos/trip.jpg",
                    b"trip.jpg".as_slice(),
                    "jpg",
                    "image",
                    2048,
                    1_700_000_000_000_000_000i64,
                    1_710_000_000_000_000_000i64,
                    "trip.jpg",
                    "holiday home",
                    "/media/photos"
                ],
            )
            .unwrap();
        let file_id = index.connection.last_insert_rowid();
        for root_id in ["child", "parent"] {
            index
                .connection
                .execute(
                    "INSERT INTO root_files (root_id, file_id, scan_generation)
                     VALUES (?1, ?2, 7)",
                    (root_id, file_id),
                )
                .unwrap();
        }

        let files: i64 = index
            .connection
            .query_row("SELECT count(*) FROM files", [], |row| row.get(0))
            .unwrap();
        let memberships: i64 = index
            .connection
            .query_row("SELECT count(*) FROM root_files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(files, 1);
        assert_eq!(memberships, 2);
    }

    #[test]
    fn file_search_text_is_kept_in_sync_by_the_schema() {
        let (_directory, index) = open_index();
        index
            .connection
            .execute(
                "INSERT INTO files (
                    path, display_path, file_name, extension, media_type,
                    size_bytes, created_at_ns, modified_at_ns,
                    file_name_search, tags_search, folders_search
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    b"/media/photos/trip.jpg".as_slice(),
                    "/media/photos/trip.jpg",
                    b"trip.jpg".as_slice(),
                    "jpg",
                    "image",
                    2048,
                    1_700_000_000_000_000_000i64,
                    1_710_000_000_000_000_000i64,
                    "trip.jpg",
                    "holiday home",
                    "/media/photos"
                ],
            )
            .unwrap();

        let matches: i64 = index
            .connection
            .query_row(
                "SELECT count(*) FROM search_docs WHERE search_docs MATCH 'holiday'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(matches, 1);

        index
            .connection
            .execute(
                "UPDATE files SET tags_search = 'work' WHERE display_path = '/media/photos/trip.jpg'",
                [],
            )
            .unwrap();
        let old_matches: i64 = index
            .connection
            .query_row(
                "SELECT count(*) FROM search_docs WHERE search_docs MATCH 'holiday'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let new_matches: i64 = index
            .connection
            .query_row(
                "SELECT count(*) FROM search_docs WHERE search_docs MATCH 'work'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_matches, 0);
        assert_eq!(new_matches, 1);
    }

    #[test]
    fn file_search_text_supports_case_insensitive_substrings() {
        let (_directory, index) = open_index();
        index
            .connection
            .execute(
                "INSERT INTO files (
                    path, display_path, file_name, extension, media_type,
                    size_bytes, created_at_ns, modified_at_ns,
                    file_name_search, tags_search, folders_search
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    b"/media/photos/trip.jpg".as_slice(),
                    "/media/photos/trip.jpg",
                    b"trip.jpg".as_slice(),
                    "jpg",
                    "image",
                    2048,
                    1_700_000_000_000_000_000i64,
                    1_710_000_000_000_000_000i64,
                    "trip.jpg",
                    "HolidayHome",
                    "/media/FamilyPhotos"
                ],
            )
            .unwrap();

        for query in ["LID", "ilypho"] {
            let matches: i64 = index
                .connection
                .query_row(
                    "SELECT count(*) FROM search_docs WHERE search_docs MATCH ?1",
                    [query],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(matches, 1, "query: {query}");
        }
    }

    #[test]
    fn an_initial_scan_indexes_media_and_reports_detailed_progress() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let nested = root.join("Family Archive");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Trip [home holiday].JPG"), b"photo").unwrap();
        std::fs::write(nested.join("Clip.mp4"), b"video").unwrap();
        std::fs::write(nested.join("notes.txt"), b"text").unwrap();
        std::fs::File::create(nested.join("empty.mov")).unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        let mut updates = Vec::new();

        let finished = index
            .scan_root("root", &root, |progress| updates.push(progress.clone()))
            .unwrap();

        assert_eq!(finished.phase, ScanPhase::Ready);
        assert_eq!(finished.folders_discovered, 2);
        assert_eq!(finished.folders_scanned, 2);
        assert_eq!(finished.files_discovered, 4);
        assert_eq!(finished.files_indexed, 2);
        assert_eq!(finished.videos_indexed, 1);
        assert_eq!(finished.images_indexed, 1);
        assert_eq!(finished.skipped_paths, 1);
        assert_eq!(finished.read_failures, 0);
        assert!(finished.scan_rate > 0.0);
        assert!(finished.current_folder.is_none());
        assert!(finished.current_file.is_none());
        assert!(updates.iter().any(|progress| {
            progress.phase == ScanPhase::Scanning
                && progress.current_folder.as_deref() == Some(nested.as_path())
        }));
        assert!(updates.iter().any(|progress| {
            progress.phase == ScanPhase::Scanning
                && progress.current_file.as_deref() == Some(nested.join("Clip.mp4").as_path())
        }));

        let stored = index.scan_progress("root").unwrap().unwrap();
        assert_eq!(stored.phase, ScanPhase::Ready);
        assert_eq!(stored.files_indexed, 2);
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([
                nested.join("Clip.mp4").to_string_lossy().into_owned(),
                nested
                    .join("Trip [home holiday].JPG")
                    .to_string_lossy()
                    .into_owned(),
            ])
        );
        let tags: String = index
            .connection
            .query_row(
                "SELECT tags_search FROM files WHERE extension = 'jpg'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tags, "holiday home");
    }

    #[test]
    fn scan_batches_are_searchable_before_the_scan_finishes() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("first.mp4"), b"one").unwrap();
        std::fs::write(root.join("second.mp4"), b"two").unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut scanner = OwnedIndex::open(&database).unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        let mut partial_count = None;

        scanner
            .scan_root_with_batch_size("root", &root, 1, |progress| {
                if progress.phase == ScanPhase::Scanning && progress.files_indexed == 1 {
                    partial_count = Some(search_match_count(&observer, "mp4"));
                }
            })
            .unwrap();

        assert_eq!(partial_count, Some(1));
        assert_eq!(search_match_count(&observer, "mp4"), 2);
    }

    #[test]
    fn a_successful_rescan_removes_files_unseen_in_its_generation() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let kept = root.join("kept.mp4");
        let removed = root.join("removed.mp4");
        std::fs::write(&kept, b"keep").unwrap();
        std::fs::write(&removed, b"remove").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        std::fs::remove_file(removed).unwrap();

        index.scan_root("root", &root, |_| {}).unwrap();

        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([kept.to_string_lossy().into_owned()])
        );
        let generation: i64 = index
            .connection
            .query_row(
                "SELECT scan_generation FROM roots WHERE id = 'root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(generation, 2);
    }

    #[test]
    fn a_failed_rescan_keeps_the_last_complete_memberships() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"clip").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        std::fs::remove_dir_all(&root).unwrap();

        assert!(index.scan_root("root", &root, |_| {}).is_err());

        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([clip.to_string_lossy().into_owned()])
        );
        let failed = index.scan_progress("root").unwrap().unwrap();
        assert_eq!(failed.phase, ScanPhase::Error);
        assert_eq!(failed.read_failures, 1);
    }

    #[test]
    fn overlapping_root_scans_share_files_but_keep_both_memberships() {
        let directory = tempdir().unwrap();
        let parent = directory.path().join("media");
        let child = parent.join("photos");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("trip.jpg"), b"photo").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();

        index.scan_root("parent", &parent, |_| {}).unwrap();
        index.scan_root("child", &child, |_| {}).unwrap();

        assert_eq!(indexed_file_count(&index), 1);
        let memberships: i64 = index
            .connection
            .query_row("SELECT count(*) FROM root_files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(memberships, 2);
    }

    #[test]
    fn a_scan_never_indexes_its_own_database_files() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("clip.mp4"), b"clip").unwrap();
        let database = root.join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();

        index.scan_root("root", &root, |_| {}).unwrap();

        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([root.join("clip.mp4").to_string_lossy().into_owned()])
        );
    }

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !condition() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            condition(),
            "index change did not arrive within five seconds"
        );
    }

    fn indexed_size(index: &OwnedIndex, path: &Path) -> Option<i64> {
        index
            .connection
            .query_row(
                "SELECT size_bytes FROM files WHERE path = ?1",
                [path_bytes(path)],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    fn indexed_file_count(index: &OwnedIndex) -> i64 {
        index
            .connection
            .query_row("SELECT count(*) FROM files", [], |row| row.get(0))
            .unwrap()
    }

    fn search_match_count(index: &OwnedIndex, query: &str) -> i64 {
        index
            .connection
            .query_row(
                "SELECT count(*) FROM search_docs WHERE search_docs MATCH ?1",
                [query],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn indexed_display_paths(index: &OwnedIndex) -> BTreeSet<String> {
        index
            .connection
            .prepare("SELECT display_path FROM files")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn app_mutations_update_the_owned_index_before_returning() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        drop(index);
        let updates = OwnedIndexUpdates::new(database.clone());

        let tagged = tags::add(&clip, &["beach".into()]).unwrap().path;
        updates.synchronize_rename(&clip, &tagged).unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        assert_eq!(search_match_count(&observer, "beach"), 1);
        assert!(!indexed_display_paths(&observer).contains(clip.to_string_lossy().as_ref()));
        drop(observer);

        std::fs::remove_file(&tagged).unwrap();
        updates.synchronize_path(&tagged).unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        assert!(indexed_display_paths(&observer).is_empty());
        drop(observer);

        std::fs::write(&tagged, b"restored").unwrap();
        updates.synchronize_path(&tagged).unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        assert!(indexed_display_paths(&observer).contains(tagged.to_string_lossy().as_ref()));
    }

    #[test]
    fn a_rescan_flag_repairs_events_the_watcher_may_have_missed() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let missed = root.join("missed.mp4");
        std::fs::write(&missed, b"video").unwrap();
        let event = Event::new(EventKind::Any).set_flag(notify::event::Flag::Rescan);
        let mut state = NotifyEventState::default();

        apply_notify_event(&mut index, "root", &root, &mut state, event).unwrap();

        assert!(indexed_display_paths(&index).contains(missed.to_string_lossy().as_ref()));
    }

    #[test]
    fn production_watcher_starts_recursively_and_completes_its_first_scan() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("existing.mp4"), b"video").unwrap();
        let database = directory.path().join("index.sqlite3");

        let watcher = OwnedIndexWatcher::start(&database, "root".into(), root).unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        wait_for(|| {
            observer
                .scan_progress("root")
                .unwrap()
                .is_some_and(|progress| progress.phase == ScanPhase::Ready)
        });

        assert_eq!(indexed_file_count(&observer), 1);
        watcher.stop().unwrap();
    }

    #[test]
    fn manager_state_restores_folder_watchers_after_restart() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let database = directory.path().join("index.sqlite3");
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        OwnedIndex::open(&database)
            .unwrap()
            .insert_root("root", &root)
            .unwrap();
        let restored_manager = OwnedIndexManager::new(database, true);
        restored_manager.restore_watchers();

        wait_for(|| {
            restored_manager
                .state()
                .is_ok_and(|state| state.folders[0].status == OwnedFolderStatus::Ready)
        });
        assert_eq!(restored_manager.state().unwrap().indexed_videos, 1);
        restored_manager.remove_folder("root").unwrap();
    }

    #[test]
    fn removing_a_missing_root_is_reported_as_an_error() {
        let (_directory, mut index) = open_index();

        assert!(matches!(
            index.remove_root("missing"),
            Err(OwnedIndexError::FolderNotConfigured)
        ));
    }

    #[test]
    fn recursive_watcher_applies_create_modify_rename_and_remove_promptly() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let database = directory.path().join("index.sqlite3");
        let (watcher, events) = OwnedIndexWatcher::start_with_test_events(
            &database,
            "root".into(),
            root.clone(),
            |_, _| {},
        )
        .unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        wait_for(|| {
            observer
                .scan_progress("root")
                .unwrap()
                .is_some_and(|progress| progress.phase == ScanPhase::Ready)
        });
        let clip = nested.join("clip.mp4");

        std::fs::write(&clip, b"one").unwrap();
        events
            .send(Ok(Event::new(EventKind::Create(
                notify::event::CreateKind::File,
            ))
            .add_path(clip.clone())))
            .unwrap();
        wait_for(|| indexed_size(&observer, &clip) == Some(3));

        std::fs::write(&clip, b"a larger video").unwrap();
        events
            .send(Ok(Event::new(EventKind::Modify(
                notify::event::ModifyKind::Data(notify::event::DataChange::Content),
            ))
            .add_path(clip.clone())))
            .unwrap();
        wait_for(|| indexed_size(&observer, &clip) == Some(14));

        let renamed = root.join("renamed");
        std::fs::rename(&nested, &renamed).unwrap();
        let moved = renamed.join("clip.mp4");
        events
            .send(Ok(Event::new(EventKind::Modify(
                notify::event::ModifyKind::Name(notify::event::RenameMode::Both),
            ))
            .add_path(nested)
            .add_path(renamed.clone())))
            .unwrap();
        wait_for(|| indexed_size(&observer, &moved) == Some(14));

        std::fs::remove_dir_all(&renamed).unwrap();
        events
            .send(Ok(Event::new(EventKind::Remove(
                notify::event::RemoveKind::Folder,
            ))
            .add_path(renamed)))
            .unwrap();
        wait_for(|| indexed_size(&observer, &moved).is_none());
        watcher.stop().unwrap();
    }

    #[test]
    fn synchronizing_a_folder_adds_and_removes_its_whole_media_tree() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let added = root.join("added");
        std::fs::create_dir_all(added.join("nested")).unwrap();
        std::fs::write(added.join("one.mp4"), b"one").unwrap();
        std::fs::write(added.join("nested/two.jpg"), b"two").unwrap();

        index.synchronize_path("root", &root, &added).unwrap();
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([
                added.join("one.mp4").to_string_lossy().into_owned(),
                added.join("nested/two.jpg").to_string_lossy().into_owned(),
            ])
        );

        std::fs::remove_dir_all(&added).unwrap();
        index.synchronize_path("root", &root, &added).unwrap();
        assert!(indexed_display_paths(&index).is_empty());
    }

    #[test]
    fn watcher_buffers_events_while_the_initial_scan_runs() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("existing.mp4"), b"video").unwrap();
        let database = directory.path().join("index.sqlite3");
        let late = root.join("late.jpg");
        let late_from_scan = late.clone();
        let nested_from_scan = nested.clone();

        let (watcher, _events) = OwnedIndexWatcher::start_with_test_events(
            &database,
            "root".into(),
            root.clone(),
            move |progress, events| {
                if progress.phase == ScanPhase::Scanning
                    && progress.current_folder.as_deref() == Some(nested_from_scan.as_path())
                    && !late_from_scan.exists()
                {
                    std::fs::write(&late_from_scan, b"photo").unwrap();
                    events
                        .send(Ok(Event::new(EventKind::Create(
                            notify::event::CreateKind::File,
                        ))
                        .add_path(late_from_scan.clone())))
                        .unwrap();
                }
            },
        )
        .unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        wait_for(|| indexed_display_paths(&observer).contains(late.to_string_lossy().as_ref()));

        watcher.stop().unwrap();
    }

    #[test]
    fn a_failed_direct_update_is_saved_for_repair_after_restart() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();

        assert!(index
            .synchronize_path("missing-root", &root, &clip)
            .is_err());
        drop(index);

        let mut reopened = OwnedIndex::open(&database).unwrap();
        let repairs = reopened.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, clip);
        assert_eq!(repairs[0].reason, "direct update");
        assert!(repairs[0].last_error.is_some());

        assert_eq!(reopened.repair_pending("root", &root).unwrap(), 1);
        assert!(reopened.pending_repairs().unwrap().is_empty());
    }

    #[test]
    fn paired_rename_events_move_a_directory_as_one_index_change() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let from = root.join("before");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("clip.mp4"), b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let to = root.join("after");
        std::fs::rename(&from, &to).unwrap();
        let mut state = NotifyEventState::default();

        let from_event = Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::From,
        )))
        .add_path(from.clone())
        .set_tracker(7);
        apply_notify_event(&mut index, "root", &root, &mut state, from_event).unwrap();
        assert!(indexed_display_paths(&index)
            .contains(from.join("clip.mp4").to_string_lossy().as_ref()));

        let to_event = Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::To,
        )))
        .add_path(to.clone())
        .set_tracker(7);
        apply_notify_event(&mut index, "root", &root, &mut state, to_event).unwrap();
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([to.join("clip.mp4").to_string_lossy().into_owned()])
        );
    }

    #[test]
    fn a_failed_direct_rename_is_saved_for_repair() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let from = root.join("clip.mp4");
        let to = root.join("clip [home].mp4");
        std::fs::write(&from, b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        std::fs::rename(&from, &to).unwrap();

        assert!(index
            .synchronize_rename("missing-root", &root, &from, &to)
            .is_err());

        let repairs = index.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, to);
        assert_eq!(repairs[0].reason, "direct rename");

        assert_eq!(index.repair_pending("root", &root).unwrap(), 1);
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([to.to_string_lossy().into_owned()])
        );
        assert!(index.pending_repairs().unwrap().is_empty());
    }

    #[test]
    fn renaming_an_indexed_root_moves_its_files_and_root_record() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let renamed = directory.path().join("media [home]");
        std::fs::rename(&root, &renamed).unwrap();

        index
            .synchronize_rename("root", &root, &root, &renamed)
            .unwrap();

        assert_eq!(
            index.roots().unwrap(),
            vec![("root".into(), renamed.clone())]
        );
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([renamed.join("clip.mp4").to_string_lossy().into_owned()])
        );
    }

    #[test]
    fn a_rename_crossing_a_root_edge_removes_or_adds_membership() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let outside = directory.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let inside = root.join("clip.mp4");
        let moved_out = outside.join("clip.mp4");
        std::fs::write(&inside, b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();

        std::fs::rename(&inside, &moved_out).unwrap();
        index
            .synchronize_rename("root", &root, &inside, &moved_out)
            .unwrap();
        assert!(indexed_display_paths(&index).is_empty());

        std::fs::rename(&moved_out, &inside).unwrap();
        index
            .synchronize_rename("root", &root, &moved_out, &inside)
            .unwrap();
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([inside.to_string_lossy().into_owned()])
        );
    }

    #[test]
    fn a_rename_reconciles_overlapping_root_memberships() {
        let directory = tempdir().unwrap();
        let parent = directory.path().join("media");
        let child = parent.join("photos");
        std::fs::create_dir_all(&child).unwrap();
        let from = child.join("trip.jpg");
        let to = parent.join("trip.jpg");
        std::fs::write(&from, b"photo").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.scan_root("parent", &parent, |_| {}).unwrap();
        index.scan_root("child", &child, |_| {}).unwrap();
        std::fs::rename(&from, &to).unwrap();

        index
            .synchronize_rename("parent", &parent, &from, &to)
            .unwrap();

        let parent_memberships: i64 = index
            .connection
            .query_row(
                "SELECT count(*) FROM root_files WHERE root_id = 'parent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let child_memberships: i64 = index
            .connection
            .query_row(
                "SELECT count(*) FROM root_files WHERE root_id = 'child'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(parent_memberships, 1);
        assert_eq!(child_memberships, 0);
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([to.to_string_lossy().into_owned()])
        );
    }

    #[test]
    fn synchronizing_a_directory_rename_moves_every_child_in_one_change() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let old_folder = root.join("beforeqzxv-folder");
        std::fs::create_dir_all(old_folder.join("nested")).unwrap();
        std::fs::write(old_folder.join("one.mp4"), b"one").unwrap();
        std::fs::write(old_folder.join("nested/two.jpg"), b"two").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let new_folder = root.join("afterqzxv-folder");

        std::fs::rename(&old_folder, &new_folder).unwrap();
        index
            .synchronize_rename("root", &root, &old_folder, &new_folder)
            .unwrap();

        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([
                new_folder.join("one.mp4").to_string_lossy().into_owned(),
                new_folder
                    .join("nested/two.jpg")
                    .to_string_lossy()
                    .into_owned(),
            ])
        );
        assert_eq!(search_match_count(&index, "beforeqzxv"), 0);
        assert_eq!(search_match_count(&index, "afterqzxv"), 2);
    }

    #[test]
    fn synchronizing_a_path_applies_create_modify_and_remove_before_returning() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let clip = root.join("clip.mp4");

        std::fs::write(&clip, b"one").unwrap();
        index.synchronize_path("root", &root, &clip).unwrap();
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([clip.to_string_lossy().into_owned()])
        );

        std::fs::write(&clip, b"a larger video").unwrap();
        index.synchronize_path("root", &root, &clip).unwrap();
        let size: i64 = index
            .connection
            .query_row("SELECT size_bytes FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(size, 14);

        std::fs::remove_file(&clip).unwrap();
        index.synchronize_path("root", &root, &clip).unwrap();
        assert!(indexed_display_paths(&index).is_empty());
    }

    #[test]
    fn a_managed_root_reports_scanned_media_and_folder_state() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        std::fs::write(root.join("photo.jpg"), b"image").unwrap();
        let manager = OwnedIndexManager::under(directory.path().join("index.sqlite3"));

        let added = manager.add_folder(&root).unwrap();
        assert_eq!(added.folders.len(), 1);
        assert_eq!(added.folders[0].status, OwnedFolderStatus::Pending);

        let id = added.folders[0].id.clone();
        OwnedIndex::open(&manager.database)
            .unwrap()
            .scan_root(&id, &root, |_| {})
            .unwrap();

        let state = manager.state().unwrap();
        assert!(state.supported);
        assert_eq!(state.indexed_videos, 1);
        assert_eq!(state.indexed_images, 1);
        assert_eq!(state.folders[0].status, OwnedFolderStatus::Ready);
        assert_eq!(state.folders[0].scanned_files, 2);
    }

    #[test]
    fn adding_and_removing_a_search_folder_advances_the_index_revision() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let manager = OwnedIndexManager::under(directory.path().join("index.sqlite3"));
        let before = manager.state().unwrap().revision;

        let added = manager.add_folder(&root).unwrap();
        let removed = manager.remove_folder(&added.folders[0].id).unwrap();

        assert!(added.revision > before);
        assert!(removed.revision > added.revision);
    }

    #[test]
    fn a_missing_managed_root_is_reported_offline() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let manager = OwnedIndexManager::under(directory.path().join("index.sqlite3"));
        manager.add_folder(&root).unwrap();
        std::fs::remove_dir(&root).unwrap();

        let state = manager.state().unwrap();
        assert_eq!(state.folders[0].status, OwnedFolderStatus::Offline);
    }

    #[test]
    fn removing_a_managed_root_removes_its_files() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        let manager = OwnedIndexManager::under(directory.path().join("index.sqlite3"));
        let id = manager.add_folder(&root).unwrap().folders[0].id.clone();
        OwnedIndex::open(&manager.database)
            .unwrap()
            .scan_root(&id, &root, |_| {})
            .unwrap();

        let state = manager.remove_folder(&id).unwrap();

        assert!(state.folders.is_empty());
        assert_eq!(state.indexed_videos, 0);
    }

    #[test]
    fn overlapping_managed_roots_are_rejected() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let manager = OwnedIndexManager::under(directory.path().join("index.sqlite3"));
        manager.add_folder(&root).unwrap();

        let error = manager.add_folder(&child).unwrap_err();

        assert_eq!(
            error,
            "That folder is already covered by a Toka search folder."
        );
        assert_eq!(manager.state().unwrap().folders.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn the_database_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().unwrap();
        let path = directory.path().join("index.sqlite3");
        let index = OwnedIndex::open(&path).unwrap();
        index
            .connection
            .execute(
                "INSERT INTO roots (id, path, display_path) VALUES ('root', x'2f', '/')",
                [],
            )
            .unwrap();

        for entry in std::fs::read_dir(directory.path()).unwrap() {
            let entry = entry.unwrap();
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("index.sqlite3")
            {
                continue;
            }
            let mode = entry.metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{} was not private", entry.path().display());
        }
    }
}
