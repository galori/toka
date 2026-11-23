use crate::{
    index_visibility,
    search::{is_supported_image, is_supported_video},
    tags,
    timestamp::{now_ns, time_ns},
};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{
    params, Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
};
use serde::Serialize;
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use std::sync::Mutex;

const SCHEMA_VERSION: i64 = 2;
const REPAIR_SCAN_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// Media files committed per scan transaction.
const SCAN_BATCH_SIZE: usize = 128;

const SCHEMA: &str = r#"
CREATE TABLE schema_meta (
    key TEXT PRIMARY KEY,
    value INTEGER NOT NULL
) WITHOUT ROWID;

INSERT INTO schema_meta (key, value) VALUES
    ('schema_version', 2),
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
    files_indexable INTEGER NOT NULL DEFAULT 0,
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

CREATE TABLE daemon_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    pid INTEGER NOT NULL,
    started_at_ns INTEGER NOT NULL,
    heartbeat_at_ns INTEGER NOT NULL,
    activity TEXT NOT NULL CHECK (activity IN ('idle', 'scanning', 'repairing', 'thumbnails', 'offline-waiting')),
    current_root BLOB,
    current_path BLOB,
    pending_roots INTEGER NOT NULL DEFAULT 0,
    files_remaining INTEGER NOT NULL DEFAULT 0,
    pending_repairs INTEGER NOT NULL DEFAULT 0,
    pending_thumbnails INTEGER NOT NULL DEFAULT 0
);
"#;

/// Version 1 databases predate the daemon liveness row and the indexable
/// file counter; the migration only adds the table and the column, so
/// existing roots, files, and scan progress survive untouched and no
/// re-index is required.
const MIGRATION_1_TO_2: &str = r#"
ALTER TABLE scan_runs ADD COLUMN files_indexable INTEGER NOT NULL DEFAULT 0;

CREATE TABLE daemon_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    pid INTEGER NOT NULL,
    started_at_ns INTEGER NOT NULL,
    heartbeat_at_ns INTEGER NOT NULL,
    activity TEXT NOT NULL CHECK (activity IN ('idle', 'scanning', 'repairing', 'thumbnails', 'offline-waiting')),
    current_root BLOB,
    current_path BLOB,
    pending_roots INTEGER NOT NULL DEFAULT 0,
    files_remaining INTEGER NOT NULL DEFAULT 0,
    pending_repairs INTEGER NOT NULL DEFAULT 0,
    pending_thumbnails INTEGER NOT NULL DEFAULT 0
);

UPDATE schema_meta SET value = 2 WHERE key = 'schema_version';
"#;

#[derive(Debug, thiserror::Error)]
pub enum OwnedIndexError {
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
    #[error("Toka stopped indexing {path} early.")]
    Cancelled { path: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanPhase {
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
pub struct ScanProgress {
    pub root_id: String,
    pub phase: ScanPhase,
    pub folders_discovered: u64,
    pub folders_scanned: u64,
    pub files_discovered: u64,
    pub files_indexable: u64,
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
            files_indexable: 0,
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
        let folders_search = folders_search_text(&path);

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

pub struct OwnedIndex {
    connection: Connection,
    path: PathBuf,
    /// The `user_version` this handle opened at. A migrating open always
    /// normalizes to [`SCHEMA_VERSION`]; a read-only open keeps whatever it
    /// found, so readers can tolerate older schemas (no `daemon_state`
    /// table or `files_indexable` column on version 1, no tables at all on
    /// a version 0 empty file) instead of erroring on them.
    opened_version: i64,
}

/// A root's recorded mount columns. `None` fields mean nothing was recorded
/// yet, not that the values are empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StoredMount {
    pub target: Option<PathBuf>,
    pub source: Option<String>,
    pub uuid: Option<String>,
}

#[derive(Clone)]
pub struct OwnedIndexUpdates {
    database: Option<PathBuf>,
}

/// What a sync matched. The app ignores the outcome — its paths always
/// come from the index — while `ts` turns `Unmatched` into an honest
/// error instead of a silent no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// The paths sit under a watched folder; the sync was applied.
    Matched,
    /// The paths sit under no watched folder; the index is untouched.
    Unmatched,
    /// There is no index yet, and a sync never creates one.
    NoIndex,
}

pub trait IndexUpdates: Send + Sync {
    /// Refreshes one path after it changed on disk; reports whether a watched folder matched.
    fn synchronize_path(&self, path: &Path) -> Result<SyncOutcome, OwnedIndexError>;
    /// Moves one indexed path to its new name; both sides must sit under one watched folder,
    /// except that renaming a watched folder itself re-points its roots row and `to` may sit
    /// outside the old root.
    fn synchronize_rename(&self, from: &Path, to: &Path) -> Result<SyncOutcome, OwnedIndexError>;
}

/// The index's spelling of `path`: canonical when the path exists,
/// otherwise the canonical nearest existing ancestor with the missing
/// remainder re-joined. Shell callers hand over paths as typed —
/// relative, with `.`/`..`, through symlinks — while matching and
/// storage assume canonical spellings; normalizing here keeps a
/// relative sync from silently missing and a non-canonical one from
/// storing a second row for the same file. A no-op for callers that
/// already pass canonical paths. Empty input returns as-is: resolving
/// it against the working directory would silently retarget the call
/// to whatever the shell happened to be standing in.
pub fn canonical_spelling(path: &Path) -> PathBuf {
    if path.as_os_str().is_empty() {
        return path.to_path_buf();
    }
    let once = canonical_spelling_once(path);
    // A `..` past a missing part can hide a symlink the first walk
    // never saw (`R/gone/../link/…` rebuilds to `R/link/…` with the
    // link unresolved): re-resolving the rebuilt path lets its existing
    // ancestors canonicalize too. One more pass suffices — the rebuilt
    // path is lexically clean, so the second walk only strips genuinely
    // missing tails and the result is already a fixpoint.
    let twice = canonical_spelling_once(&once);
    if twice == once {
        once
    } else {
        twice
    }
}

fn canonical_spelling_once(path: &Path) -> PathBuf {
    use std::path::Component;

    // Fail closed, as in `canonical_spelling`: the invariant lives with
    // the walker so no caller can resolve "" against the working directory.
    if path.as_os_str().is_empty() {
        return path.to_path_buf();
    }
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    // A deleted file or an already-moved rename source has no canonical
    // form of its own; resolve what is still there and re-join the rest.
    // Each ancestor goes to the OS first, so `..` past parts that exist
    // resolves with symlinks honored; only `..` past truly missing parts
    // falls back to lexical meaning in the final cleanup.
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut missing: Vec<OsString> = Vec::new();
    let mut ancestor: &Path = &absolute;
    loop {
        if let Ok(canonical) = ancestor.canonicalize() {
            let mut spelling = canonical;
            for component in missing.into_iter().rev() {
                spelling.push(component);
            }
            return normalize_lexically(&spelling);
        }
        // One raw component at a time — `..` included, which `file_name`
        // would silently drop — so the walk cannot lose climbs.
        let stripped = ancestor.components().next_back();
        let parent = ancestor
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
        match (stripped, parent) {
            (Some(Component::Normal(name)), Some(parent)) => {
                missing.push(name.to_os_string());
                ancestor = parent;
            }
            (Some(Component::ParentDir), Some(parent)) => {
                missing.push(OsString::from(".."));
                ancestor = parent;
            }
            (Some(Component::CurDir), Some(parent)) => {
                ancestor = parent;
            }
            // No existing ancestor (only when the current directory is
            // unreadable and the path is relative): the cleaned-up
            // spelling is still absolute-or-as-typed and deterministic.
            _ => return normalize_lexically(&absolute),
        }
    }
}

/// Resolves `.` and `..` without touching the filesystem, for paths
/// with missing parts that `canonicalize` refuses. A `..` past the
/// root of an absolute path is dropped; one a relative path cannot
/// climb out of survives, so the result keeps its meaning.
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => normalized.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                // `pop` fails on a root (leave it: nothing to climb)
                // and on an empty path (keep the `..`: relative).
                if !normalized.pop() && !normalized.is_absolute() {
                    normalized.push("..");
                }
            }
            Component::Normal(_) => normalized.push(component),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

/// Whether a roots row other than `root_id` already holds `path`: the
/// aliased-twin layout the migration deliberately preserves instead of
/// merging. Takes the connection so both plain reads and the migration's
/// write transaction (which derefs to one) share this check.
fn another_root_holds(
    connection: &Connection,
    root_id: &str,
    path: &Path,
) -> Result<bool, OwnedIndexError> {
    let collision: Option<String> = connection
        .query_row(
            "SELECT id FROM roots WHERE path = ?1 AND id != ?2",
            params![path_bytes(path), root_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(collision.is_some())
}

impl OwnedIndexUpdates {
    // Unconditional (not just `cfg(test)`): the app's tests build updates too,
    // and this crate compiles without `cfg(test)` when they do.
    pub fn new(database: PathBuf) -> Self {
        Self {
            database: Some(database),
        }
    }

    pub fn system() -> Self {
        Self {
            database: database_path().ok(),
        }
    }

    /// The database to sync against, if there is one. Only a truly
    /// missing index is a silent no-op: `is_file` is also false when the
    /// metadata itself cannot be read (an unsearchable parent, a denied
    /// path), and those failures are errors, never quiet skips.
    fn index_file(&self) -> Result<Option<&Path>, OwnedIndexError> {
        let Some(database) = self.database.as_deref() else {
            return Ok(None);
        };
        match fs::metadata(database) {
            Ok(metadata) if metadata.is_file() => Ok(Some(database)),
            // A directory (or anything else) where the database belongs
            // is not a missing index: opening it below fails honestly.
            Ok(_) => Ok(Some(database)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(OwnedIndexError::PathRead {
                path: database.display().to_string(),
                source,
            }),
        }
    }

    pub(crate) fn synchronize_path(&self, path: &Path) -> Result<SyncOutcome, OwnedIndexError> {
        let Some(database) = self.index_file()? else {
            return Ok(SyncOutcome::NoIndex);
        };
        let path = canonical_spelling(path);
        let mut index = OwnedIndex::open(database)?;
        // Recorded roots are canonical in production (`add_folder`
        // stores nothing else); a row that is not — re-pointed
        // through a symlinked parent, say — migrates to the
        // canonical spelling on first touch, so lookups and storage
        // never disagree about one file for long. Heal in memory
        // first and migrate only the roots this sync matches, so an
        // unmatched sync leaves the index untouched.
        let roots = index.roots()?;
        let healed_roots: Vec<(String, PathBuf, PathBuf)> = roots
            .into_iter()
            .map(|(id, root)| {
                let healed = canonical_spelling(&root);
                (id, root, healed)
            })
            .collect();
        let mut matched = false;
        for (root_id, recorded, healed) in &healed_roots {
            if path.starts_with(healed) {
                if *healed != *recorded {
                    index.migrate_root_spelling(root_id, recorded, healed)?;
                }
                // A one-shot CLI sync has no stop request to honor.
                index.synchronize_path(root_id, healed, &path, &AtomicBool::new(false))?;
                matched = true;
            }
        }
        Ok(if matched {
            SyncOutcome::Matched
        } else {
            SyncOutcome::Unmatched
        })
    }

    pub(crate) fn synchronize_rename(
        &self,
        from: &Path,
        to: &Path,
    ) -> Result<SyncOutcome, OwnedIndexError> {
        let Some(database) = self.index_file()? else {
            return Ok(SyncOutcome::NoIndex);
        };
        let from = canonical_spelling(from);
        let to = canonical_spelling(to);
        let mut index = OwnedIndex::open(database)?;
        // Same migration as `synchronize_path`: one path identity
        // for lookups and storage, whichever sync touches it first —
        // but only for the root this rename matches, so an unmatched
        // rename migrates nothing.
        let roots = index.roots()?;
        let healed_roots: Vec<(String, PathBuf, PathBuf)> = roots
            .into_iter()
            .map(|(id, root)| {
                let healed = canonical_spelling(&root);
                (id, root, healed)
            })
            .collect();
        // Exact before containing: renaming a nested watched root must
        // resolve to its own row, not to whichever containing root the
        // id order happens to list first.
        let matched = healed_roots
            .iter()
            .find(|(_, _, healed)| from == *healed)
            .or_else(|| {
                healed_roots
                    .iter()
                    .find(|(_, _, healed)| from.starts_with(healed) && to.starts_with(healed))
            });
        match matched {
            Some((root_id, recorded, healed)) => {
                if *healed != *recorded {
                    index.migrate_root_spelling(root_id, recorded, healed)?;
                }
                // A one-shot CLI sync has no stop request to honor.
                index.synchronize_rename(root_id, healed, &from, &to, &AtomicBool::new(false))?;
                Ok(SyncOutcome::Matched)
            }
            None => Ok(SyncOutcome::Unmatched),
        }
    }
}

impl IndexUpdates for OwnedIndexUpdates {
    fn synchronize_path(&self, path: &Path) -> Result<SyncOutcome, OwnedIndexError> {
        OwnedIndexUpdates::synchronize_path(self, path)
    }

    fn synchronize_rename(&self, from: &Path, to: &Path) -> Result<SyncOutcome, OwnedIndexError> {
        OwnedIndexUpdates::synchronize_rename(self, from, to)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OwnedFolderStatus {
    Pending,
    Indexing,
    Ready,
    Offline,
    Error,
}

/// One directory the indexer has visited, with what it found directly in it.
/// `scanning` is true for the directory being read right now.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannedDirectory {
    #[serde(serialize_with = "crate::lossy_path::serialize")]
    pub path: PathBuf,
    pub scanning: bool,
    pub videos: u64,
    pub images: u64,
    pub subfolders: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnedIndexFolder {
    pub id: String,
    // Lossy on purpose: real paths can hold non-UTF-8 bytes on Unix, and
    // those must not break serialization of this response.
    #[serde(serialize_with = "crate::lossy_path::serialize")]
    pub path: PathBuf,
    pub status: OwnedFolderStatus,
    pub message: Option<String>,
    pub scanned_files: u64,
    /// The directory the scan is reading right now; only set while indexing.
    /// A lossy display hint on purpose: real paths can hold non-UTF-8 bytes
    /// on Unix, and those must not break serialization of this response.
    pub current_path: Option<String>,
    /// Recently visited directories. The owned scanner does not track
    /// per-directory tallies yet, so this stays empty until that lands; the
    /// status panel already tolerates an empty list.
    pub recent_directories: Vec<ScannedDirectory>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnedIndexState {
    pub supported: bool,
    pub revision: u64,
    pub indexed_videos: u64,
    pub indexed_images: u64,
    pub indexing_files: u64,
    pub folders: Vec<OwnedIndexFolder>,
}

pub struct OwnedIndexManager {
    database: PathBuf,
    watch: bool,
    read_only: bool,
    watchers: Mutex<HashMap<String, ManagedRootWatcher>>,
    // Behind a mutex: a manager that yielded to a running daemon retries
    // the acquisition on every reconcile, so supervision resumes here
    // once the daemon stops.
    daemon_guard: Mutex<Option<crate::indexer_daemon::DaemonLock>>,
}

struct ManagedRootWatcher {
    path: PathBuf,
    watcher: OwnedIndexWatcher,
}

/// Whether an inspectable index file exists: only a truly missing index
/// reads empty. Any other metadata failure (permissions, I/O) is an
/// error, and a non-file at the index path is a broken setup, not an
/// empty index — `is_file` answers false for all three, which used to
/// report every metadata failure as a healthy-empty index with exit 0.
/// The single taxonomy for every read command (`ts` and status alike),
/// so the exit codes cannot drift apart again.
pub fn index_file_for_inspection(database: &Path) -> Result<bool, String> {
    match fs::metadata(database) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Toka could not read its search index: {error}")),
        Ok(metadata) if !metadata.is_file() => Err(format!(
            "Toka's search index path is not a file: {}",
            database.display()
        )),
        Ok(_) => Ok(true),
    }
}

/// Creates (and secures) the index parent, so acquiring the legacy
/// daemon lock never fails on a missing folder after a reset wipes it.
/// Both the startup acquisition and a yielded manager's re-acquisition go
/// through here: the legacy lock file lives beside the index (the primary
/// one lives in the runtime directory), and either path can run with the
/// whole folder gone.
pub(crate) fn ensure_index_parent(database: &Path) -> Result<(), String> {
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent).map_err(|source| {
            OwnedIndexError::Permissions {
                path: parent.display().to_string(),
                source,
            }
            .to_string()
        })?;
        secure_directory(parent).map_err(|error| error.to_string())?;
    }
    Ok(())
}

impl OwnedIndexManager {
    #[cfg(any(target_os = "macos", test))]
    pub fn system() -> Result<Self, String> {
        Self::system_under(database_path()?)
    }

    /// A watching manager on an explicit database: what `system` starts,
    /// and what tests supervise without touching the real index.
    #[cfg(any(target_os = "macos", test))]
    fn system_under(database: PathBuf) -> Result<Self, String> {
        // Create (and secure) the legacy lock's parent first, so a first
        // launch — or a post-reset one, where the whole folder is gone —
        // never fails opening the lock. Acquisition itself still happens
        // before any watcher starts.
        ensure_index_parent(&database)?;
        // One supervisor per index: when a `ts` daemon already runs here,
        // the app leaves supervision to it instead of interleaving scan
        // generations with its own watchers.
        let daemon_guard = crate::indexer_daemon::try_acquire_daemon_lock(&database)?;
        if daemon_guard.is_none() {
            eprintln!(
                "Toka's indexer daemon is already running: leaving search folder supervision to it."
            );
        }
        let mut manager = Self::new(database, true);
        manager.daemon_guard = Mutex::new(daemon_guard);
        manager.restore_watchers();
        Ok(manager)
    }

    /// A manager for reading the index without starting watchers.
    pub fn for_inspection() -> Result<Self, String> {
        Ok(Self::new(database_path()?, false))
    }

    /// A manager for `ts` reads that must leave the index unchanged:
    /// every open is read-only, so a version 1 database is never
    /// migrated and stays usable by a version 1 app. Anything that
    /// writes through this manager fails loudly on the query-only
    /// connection instead of slipping through; `ts` wires only its read
    /// commands to it. (SQLite may still touch `-wal`/`-shm` sidecars
    /// beside the database while reading.)
    pub fn for_read_only_inspection() -> Result<Self, String> {
        let mut manager = Self::new(database_path()?, false);
        manager.read_only = true;
        Ok(manager)
    }

    /// A read-only manager on an explicit database, for tests. Mirrors
    /// [`under`]: no watchers, and every open read-only.
    pub fn under_read_only(database: PathBuf) -> Self {
        let mut manager = Self::new(database, false);
        manager.read_only = true;
        manager
    }

    // Unconditional (not just `cfg(test)`): the app's tests build managers
    // too, and this crate compiles without `cfg(test)` when they do.
    /// A manager rooted at an explicit database, without watchers. Exists
    /// for tests and inspection flows; prefer `for_inspection`/`system`.
    pub fn under(database: PathBuf) -> Self {
        Self::new(database, false)
    }

    fn new(database: PathBuf, watch: bool) -> Self {
        Self {
            database,
            watch,
            read_only: false,
            watchers: Mutex::new(HashMap::new()),
            daemon_guard: Mutex::new(None),
        }
    }

    /// Whether this manager runs its own watchers: a system manager yields
    /// to a running `ts` daemon, which holds the same lock. Reads keep
    /// working either way; only supervision is skipped.
    fn supervises_watchers(&self) -> Result<bool, String> {
        if !self.watch {
            return Ok(false);
        }
        let guard = self
            .daemon_guard
            .lock()
            .map_err(|_| "Toka's search folder manager is unavailable.".to_owned())?;
        Ok(guard.is_some())
    }

    #[cfg(any(target_os = "macos", test))]
    fn restore_watchers(&self) {
        if let Err(error) = self.ensure_watchers() {
            eprintln!("Toka could not restore search folder watchers: {error}");
        }
    }

    pub fn state(&self) -> Result<OwnedIndexState, String> {
        self.ensure_watchers()?;
        let index = self.open().map_err(|error| error.to_string())?;
        let (indexed_videos, indexed_images) =
            index.media_counts().map_err(|error| error.to_string())?;
        let revision = index.revision().map_err(|error| error.to_string())?;
        let roots = index
            .roots_with_state()
            .map_err(|error| error.to_string())?;
        let mut folders = Vec::new();
        let mut indexing_files = 0;

        for (id, path, stored) in roots {
            let progress = index
                .scan_progress(&id)
                .map_err(|error| error.to_string())?;
            // The daemon marks roots `offline` when their storage goes away;
            // a missing path reports the same without waiting for the daemon.
            let status = if !path.is_dir() || stored == "offline" {
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
            // The panel only shows progress for folders that are indexing;
            // anything else reports no current path.
            // Lossy on purpose: see the field's doc comment.
            let current_path = if status == OwnedFolderStatus::Indexing {
                progress.as_ref().and_then(|progress| {
                    progress
                        .current_folder
                        .as_deref()
                        .map(|folder| folder.to_string_lossy().into_owned())
                })
            } else {
                None
            };
            folders.push(OwnedIndexFolder {
                id,
                path,
                status,
                message: progress.and_then(|progress| progress.error),
                scanned_files,
                current_path,
                recent_directories: Vec::new(),
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

    /// The search folders and their indexing state. Inspecting must not
    /// create an index that does not exist yet.
    pub fn folders(&self) -> Result<Vec<OwnedIndexFolder>, String> {
        if !index_file_for_inspection(&self.database)? {
            return Ok(Vec::new());
        }
        Ok(self.state()?.folders)
    }

    /// Everything the index knows, for `toka --index-stats`.
    pub fn stats(&self) -> Result<crate::index_stats::IndexStats, String> {
        use crate::index_stats::{FolderState, FolderStats, IndexStats};
        // Inspecting must not create an index that does not exist yet.
        if !index_file_for_inspection(&self.database)? {
            return Ok(IndexStats {
                revision: 0,
                videos: 0,
                images: 0,
                service_running: None,
                folders: Vec::new(),
            });
        }
        let state = self.state()?;
        let index = self.open().map_err(|error| error.to_string())?;
        let mut folders = Vec::new();
        for folder in state.folders {
            // One failed count (e.g. a busy database mid-scan) shouldn't
            // hide the rest of the report.
            let counts = index.root_media_counts(&folder.id).ok();
            folders.push(FolderStats {
                path: folder.path,
                state: match folder.status {
                    OwnedFolderStatus::Pending => FolderState::Pending,
                    OwnedFolderStatus::Indexing => FolderState::Indexing,
                    OwnedFolderStatus::Ready => FolderState::Ready,
                    OwnedFolderStatus::Offline => FolderState::Offline,
                    OwnedFolderStatus::Error => FolderState::Error,
                },
                message: folder.message,
                scanned_files: folder.scanned_files,
                videos: counts.map(|(videos, _)| videos),
                images: counts.map(|(_, images)| images),
            });
        }
        Ok(IndexStats {
            revision: state.revision,
            videos: state.indexed_videos,
            images: state.indexed_images,
            // systemd only exists on Linux; elsewhere there is no service.
            #[cfg(any(target_os = "linux", test))]
            service_running: crate::indexer_daemon::indexer_running(),
            #[cfg(not(any(target_os = "linux", test)))]
            service_running: None,
            folders,
        })
    }

    pub fn add_folder(&self, requested: &Path) -> Result<OwnedIndexState, String> {
        self.add_folder_with_committed_path(requested)
            .map(|(state, _)| state)
    }

    /// Adds a folder exactly like [`add_folder`], also reporting the spelling
    /// the manager committed to the `roots` table. The caller echoes that
    /// spelling — never its own resolution — so a symlink retargeted between
    /// the two resolutions cannot print a path the index does not contain.
    pub fn add_folder_with_committed_path(
        &self,
        requested: &Path,
    ) -> Result<(OwnedIndexState, PathBuf), String> {
        let root = requested
            .canonicalize()
            .map_err(|error| format!("Toka could not open that folder: {error}"))?;
        if !root.is_dir() {
            return Err("Choose a folder to search.".into());
        }
        // Purge and reset delete the index directory's known files: a
        // watched folder at or inside it would put media on the deletion
        // list, so the shared add path refuses it here, for the app and
        // `ts` alike.
        if let Some(parent) = self.database.parent() {
            let index_dir = canonical_spelling(parent);
            if root == index_dir {
                return Err(format!(
                    "Toka cannot search its own index folder ({}): reset and purge delete it.",
                    root.display()
                ));
            }
            if root.starts_with(&index_dir) {
                return Err(format!(
                    "Toka cannot search {}: it is inside Toka's index folder ({}), which reset and purge delete.",
                    root.display(),
                    index_dir.display()
                ));
            }
        }
        // The insert below stores exactly this; callers report it back.
        let committed = root.clone();

        let mut watchers = self
            .watchers
            .lock()
            .map_err(|_| "Toka's search folder manager is unavailable.".to_owned())?;
        let id = {
            let index = self.open().map_err(|error| error.to_string())?;
            let existing = index.roots().map_err(|error| error.to_string())?;
            if existing.iter().any(|(_, path)| {
                // Recorded rows are canonical in production, but a legacy
                // alias would otherwise slip past: compare canonical to
                // canonical so no new alias twin can be minted.
                let recorded = canonical_spelling(path);
                root.starts_with(&recorded) || recorded.starts_with(&root)
            }) {
                return Err("That folder is already covered by a Toka search folder.".into());
            }

            let id = uuid::Uuid::new_v4().to_string();
            let mut index = index;
            index
                .insert_root(&id, &root)
                .map_err(|error| error.to_string())?;
            id
        };

        if self.supervises_watchers()? {
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
        let state = self.state()?;
        Ok((state, committed))
    }

    pub fn remove_folder(&self, id: &str) -> Result<OwnedIndexState, String> {
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
        if self.read_only {
            OwnedIndex::open_read_only(&self.database)
        } else {
            OwnedIndex::open(&self.database)
        }
    }

    fn ensure_watchers(&self) -> Result<(), String> {
        // A manager that yielded to a running daemon retries the lock on
        // every pass: once the daemon stops, this manager takes over
        // supervision instead of leaving the folders unwatched for life.
        // Watchers only (re)start below, after the lock is held. The
        // legacy lock's parent is ensured first, exactly as at startup: a
        // reset while yielded wipes the index folder, and without this the
        // re-acquisition errors ENOENT on every poll until restart. (The
        // primary lock survives the reset in the runtime directory.)
        if self.watch {
            let mut guard = self
                .daemon_guard
                .lock()
                .map_err(|_| "Toka's search folder manager is unavailable.".to_owned())?;
            if guard.is_none() {
                ensure_index_parent(&self.database)?;
                *guard = crate::indexer_daemon::try_acquire_daemon_lock(&self.database)?;
            }
        }
        if !self.supervises_watchers()? {
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

pub fn database_path() -> Result<PathBuf, String> {
    dirs::data_local_dir()
        .map(|directory| {
            directory
                .join(crate::config::APP_DATA_DIR_NAME)
                .join(crate::config::INDEX_FILE_NAME)
        })
        .ok_or_else(|| "Toka could not find a place for its search index.".into())
}

/// The overlay file recording files Toka renamed since the index last saw
/// them, so searches find them by their current name. It lives beside the
/// index database; the one helper both the app and `ts` use.
pub fn search_rename_overlay_path() -> Result<PathBuf, String> {
    Ok(database_path()?.with_file_name(RENAME_OVERLAY_FILE_NAME))
}

/// The rename overlay's file name, beside the index database.
pub(crate) const RENAME_OVERLAY_FILE_NAME: &str = "search-renames.json";

/// The rename overlay lock's file name, beside the index database (see
/// `lock_rename_overlay`, which derives it the same way).
pub(crate) const RENAME_OVERLAY_LOCK_FILE_NAME: &str = "search-renames.lock";

/// The known files of the index directory: the database, its SQLite
/// sidecars, the legacy daemon lock, and the rename overlay plus its
/// lock. Purge and reset delete exactly these — never the directory
/// itself — so foreign content (including any media stored there)
/// always survives.
pub fn index_data_files(database: &Path) -> Vec<PathBuf> {
    let mut files = vec![
        database.with_file_name(crate::indexer_daemon::LEGACY_DAEMON_LOCK_FILE_NAME),
        database.with_file_name(RENAME_OVERLAY_FILE_NAME),
        database.with_file_name(RENAME_OVERLAY_LOCK_FILE_NAME),
    ];
    for suffix in ["", "-wal", "-shm", "-journal"] {
        files.push(database.with_file_name(format!("{}{suffix}", crate::config::INDEX_FILE_NAME)));
    }
    files
}

/// Opens a write transaction, taking the write lock up front. Deferred
/// transactions read first and upgrade on their first write — and when
/// that upgrade collides with another writer's commit (the daemon
/// heartbeat, a concurrent scan), the upgrade dies of BUSY_SNAPSHOT
/// instead of waiting out the busy timeout. Writers queue behind each
/// other here instead, which is what the timeout is for.
fn write_transaction(connection: &mut Connection) -> Result<Transaction<'_>, OwnedIndexError> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(OwnedIndexError::from)
}

/// Whether a SQLite error is a lock conflict worth retrying.
fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::DatabaseBusy
    )
}

/// Switches the database to WAL mode, retrying while a racing opener
/// holds the write lock. Unlike ordinary statements, `PRAGMA journal_mode`
/// does not honor the connection's busy timeout: it needs an EXCLUSIVE
/// lock and reports SQLITE_BUSY at once when a concurrent first-opener is
/// mid-migration, so the retry lives at this layer instead. Bounded like
/// the busy timeout, so a wedged lock still surfaces as an error.
fn set_wal_mode(connection: &Connection) -> Result<(), rusqlite::Error> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match connection.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(_) => return Ok(()),
            Err(error) if is_busy(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Where a sync-step probe fired: after one file's write transaction
/// committed during a sync, or while planning / rewriting an in-root
/// rename. Only constructed in tests; production builds never fire one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum SyncProbeSite {
    /// One file committed during `try_synchronize_path`, including files
    /// a directory walk reaches one by one.
    FileCommitted,
    /// One indexed row planned during `try_synchronize_rename`.
    RenamePlanned,
    /// One planned move rewritten during `try_synchronize_rename`.
    RenameRewritten,
    /// One media batch committed during a scan (`scan_root_in_batches`).
    /// Unlike [`SyncProbeSite::FileCommitted`], which fires in the
    /// single-path syncs, this is the hook scans offer: scans batch
    /// their writes and never pass through `try_synchronize_path`.
    ScanBatchCommitted,
}

/// Test-only probe fired at sync boundaries (see [`SyncProbeSite`]), so
/// cancellation tests can handshake with the worker at a commit or
/// planning boundary instead of polling the database. Thread-local, so
/// tests running in parallel never observe each other's probes;
/// production builds keep only the no-op [`fire_sync_probe`] shell.
#[cfg(test)]
type SyncStepProbeSlot = std::cell::RefCell<Option<Box<dyn Fn(SyncProbeSite)>>>;

#[cfg(test)]
thread_local! {
    static SYNC_STEP_PROBE: SyncStepProbeSlot = const { SyncStepProbeSlot::new(None) };
}

/// Installs `probe` on this thread while `run` executes, removing it
/// afterwards even when `run` panics. Only the calling thread observes
/// the probe: a worker thread installs its own.
#[cfg(test)]
pub(crate) fn with_sync_step_probe<R>(
    probe: impl Fn(SyncProbeSite) + 'static,
    run: impl FnOnce() -> R,
) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            SYNC_STEP_PROBE.with(|slot| slot.borrow_mut().take());
        }
    }
    SYNC_STEP_PROBE.with(|slot| *slot.borrow_mut() = Some(Box::new(probe)));
    let _guard = Guard;
    run()
}

/// Process-wide probe fired alongside the thread-local one, but only
/// for a single root id: watcher-driven scans run on worker threads
/// tests cannot reach, so timing-window tests handshake here instead
/// of racing wall clocks. Parallel tests use distinct root ids and
/// never observe each other's probes. Test-only; production builds
/// keep just the no-op shell below.
#[cfg(test)]
static GLOBAL_SYNC_PROBE: std::sync::Mutex<Option<GlobalSyncProbe>> = std::sync::Mutex::new(None);

#[cfg(test)]
#[derive(Clone)]
struct GlobalSyncProbe {
    root_id: String,
    // Shared so the slot lock is never held while a probe runs: a
    // gated probe blocks, and blocking under the lock would stall
    // every other test's worker at its own probe fires.
    probe: std::sync::Arc<dyn Fn(SyncProbeSite) + Send + Sync>,
}

/// Installs a process-wide probe for one root id; see
/// [`GLOBAL_SYNC_PROBE`]. Replaces any previous installation.
#[cfg(test)]
pub(crate) fn set_global_sync_probe(
    root_id: &str,
    probe: impl Fn(SyncProbeSite) + Send + Sync + 'static,
) {
    *GLOBAL_SYNC_PROBE.lock().unwrap() = Some(GlobalSyncProbe {
        root_id: root_id.to_owned(),
        probe: std::sync::Arc::new(probe),
    });
}

/// Removes the process-wide probe. Tests that install one must call
/// this before returning, so a later test never trips on a stale gate.
#[cfg(test)]
pub(crate) fn clear_global_sync_probe() {
    GLOBAL_SYNC_PROBE.lock().unwrap().take();
}

/// Runs `run` with a process-wide probe installed for one root id,
/// removing it afterwards even when `run` panics — the scoped form
/// tests should prefer over manual set/clear.
#[cfg(test)]
pub(crate) fn with_global_sync_probe<R>(
    root_id: &str,
    probe: impl Fn(SyncProbeSite) + Send + Sync + 'static,
    run: impl FnOnce() -> R,
) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            clear_global_sync_probe();
        }
    }
    set_global_sync_probe(root_id, probe);
    let _guard = Guard;
    run()
}

#[cfg(test)]
fn fire_sync_probe(root_id: &str, site: SyncProbeSite) {
    SYNC_STEP_PROBE.with(|slot| {
        if let Some(probe) = slot.borrow().as_ref() {
            probe(site);
        }
    });
    let gated = GLOBAL_SYNC_PROBE
        .lock()
        .ok()
        .and_then(|slot| slot.as_ref().map(|installed| installed.clone()));
    if let Some(installed) = gated {
        if installed.root_id == root_id {
            (installed.probe)(site);
        }
    }
}

#[cfg(not(test))]
fn fire_sync_probe(_root_id: &str, _site: SyncProbeSite) {}

/// What a watcher's worker thread is doing between filesystem events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkerStage {
    Scanning,
    Repairing,
    Watching,
}

/// A watcher's live state for the daemon heartbeat: its current stage plus
/// the latest scan progress it reported.
#[derive(Clone, Debug)]
pub(crate) struct WatcherLive {
    pub stage: WorkerStage,
    pub last_progress: Option<ScanProgress>,
}

impl Default for WatcherLive {
    fn default() -> Self {
        Self {
            stage: WorkerStage::Watching,
            last_progress: None,
        }
    }
}

/// What a watcher shares with its worker thread: the flag `stop` sets so
/// a scan or repair in progress aborts promptly, and the live snapshot
/// the daemon heartbeat reads.
#[derive(Clone)]
pub(crate) struct WorkerShared {
    cancel: Arc<AtomicBool>,
    live: Arc<Mutex<WatcherLive>>,
}

impl WorkerShared {
    fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            live: Arc::new(Mutex::new(WatcherLive::default())),
        }
    }

    fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The flag to hand to cancellable scan and repair calls.
    fn cancel_flag(&self) -> &AtomicBool {
        &self.cancel
    }

    fn set_stage(&self, stage: WorkerStage) {
        set_stage(&self.live, stage);
    }

    fn record_progress(&self, progress: &ScanProgress) {
        if let Ok(mut live) = self.live.lock() {
            live.last_progress = Some(progress.clone());
        }
    }

    fn snapshot(&self) -> WatcherLive {
        self.live
            .lock()
            .map(|live| live.clone())
            .unwrap_or_default()
    }
}

pub(crate) struct OwnedIndexWatcher {
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<Result<(), OwnedIndexError>>>,
    shared: WorkerShared,
}

/// Records a worker stage transition. Advisory only: a poisoned lock keeps
/// the last-known stage rather than crashing the worker. Entering the
/// repair stage drops the last scan's progress, so the heartbeat never
/// reports a stale file as the repair's current path; progress a repair
/// itself reports repopulates it.
fn set_stage(live: &Mutex<WatcherLive>, stage: WorkerStage) {
    if let Ok(mut live) = live.lock() {
        live.stage = stage;
        if stage == WorkerStage::Repairing {
            live.last_progress = None;
        }
    }
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
        let shared = WorkerShared::new();
        let worker_shared = shared.clone();
        // Every progress report updates the live snapshot first, so the
        // daemon heartbeat always shows what the scan last saw.
        let progress_shared = shared.clone();
        let mut on_progress = move |progress: &ScanProgress| {
            progress_shared.record_progress(progress);
            on_progress(progress);
        };
        let worker = thread::Builder::new()
            .name(format!("toka-index-{root_id}"))
            .spawn(move || {
                let _keepalive = keepalive;
                worker_shared.set_stage(WorkerStage::Scanning);
                match index.scan_root_in_batches(
                    &root_id,
                    &root,
                    SCAN_BATCH_SIZE,
                    worker_shared.cancel_flag(),
                    |progress| on_progress(progress),
                ) {
                    // A cancelled scan is a stop request honored, not a
                    // failure: exit quietly and let `stop` join promptly.
                    Err(OwnedIndexError::Cancelled { .. }) => return Ok(()),
                    result => {
                        result?;
                    }
                }
                worker_shared.set_stage(WorkerStage::Repairing);
                match index.repair_pending(
                    &root_id,
                    &root,
                    worker_shared.cancel_flag(),
                    |progress| on_progress(progress),
                ) {
                    Err(OwnedIndexError::Cancelled { .. }) => return Ok(()),
                    result => {
                        result?;
                    }
                }
                worker_shared.set_stage(WorkerStage::Watching);
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
                                &worker_shared,
                                &mut event_state,
                                event,
                                &mut on_progress,
                            ) {
                                if matches!(error, OwnedIndexError::Cancelled { .. }) {
                                    return Ok(());
                                }
                                eprintln!("Toka could not apply a search index event: {error}");
                            }
                        }
                        Ok(Err(error)) => {
                            eprintln!("Toka's folder watcher missed an event: {error}");
                            repair_root(
                                &mut index,
                                &root_id,
                                &root,
                                &worker_shared,
                                &mut on_progress,
                            );
                            last_repair_scan = Instant::now();
                        }
                        Err(mpsc::RecvTimeoutError::Timeout)
                            if last_repair_scan.elapsed() >= REPAIR_SCAN_INTERVAL =>
                        {
                            repair_root(
                                &mut index,
                                &root_id,
                                &root,
                                &worker_shared,
                                &mut on_progress,
                            );
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
            shared,
        })
    }

    pub(crate) fn stop(mut self) -> Result<(), OwnedIndexError> {
        // The flag first: a worker mid-scan aborts on it, and one already
        // in its event loop exits on the message below.
        self.shared.cancel();
        let _ = self.stop.send(());
        match self.worker.take().expect("watcher worker missing").join() {
            Ok(result) => result,
            Err(_) => Err(OwnedIndexError::WorkerStopped),
        }
    }

    /// Whether the worker thread already exited, for example after a failed
    /// scan. A finished watcher indexes nothing until it is replaced.
    pub(crate) fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
    }

    /// The worker's latest reported state, for the daemon heartbeat.
    pub(crate) fn live(&self) -> WatcherLive {
        self.shared.snapshot()
    }
}

impl Drop for OwnedIndexWatcher {
    fn drop(&mut self) {
        self.shared.cancel();
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

fn repair_root(
    index: &mut OwnedIndex,
    root_id: &str,
    root: &Path,
    shared: &WorkerShared,
    on_progress: &mut impl FnMut(&ScanProgress),
) {
    // A cancelled repair is a stop request being honored, not a failure:
    // stay silent and let the worker loop observe the stop message.
    shared.set_stage(WorkerStage::Scanning);
    if let Err(error) = index.scan_root_in_batches(
        root_id,
        root,
        SCAN_BATCH_SIZE,
        shared.cancel_flag(),
        |progress| on_progress(progress),
    ) {
        shared.set_stage(WorkerStage::Watching);
        if !matches!(error, OwnedIndexError::Cancelled { .. }) {
            eprintln!("Toka could not repair its search index: {error}");
        }
        return;
    }
    shared.set_stage(WorkerStage::Repairing);
    if let Err(error) = index.repair_pending(root_id, root, shared.cancel_flag(), |progress| {
        on_progress(progress)
    }) {
        shared.set_stage(WorkerStage::Watching);
        if !matches!(error, OwnedIndexError::Cancelled { .. }) {
            eprintln!("Toka could not replay its search index repairs: {error}");
        }
        return;
    }
    shared.set_stage(WorkerStage::Watching);
}

fn apply_notify_event(
    index: &mut OwnedIndex,
    root_id: &str,
    root: &Path,
    shared: &WorkerShared,
    state: &mut NotifyEventState,
    event: Event,
    on_progress: &mut impl FnMut(&ScanProgress),
) -> Result<(), OwnedIndexError> {
    if event.need_rescan() {
        state.renames.clear();
        shared.set_stage(WorkerStage::Scanning);
        let result = index.scan_root_in_batches(
            root_id,
            root,
            SCAN_BATCH_SIZE,
            shared.cancel_flag(),
            |progress| on_progress(progress),
        );
        shared.set_stage(WorkerStage::Watching);
        result?;
        return Ok(());
    }
    match &event.kind {
        EventKind::Modify(notify::event::ModifyKind::Name(notify::event::RenameMode::Both)) => {
            if let (Some(from), Some(to)) = (event.paths.first(), event.paths.last()) {
                return index.synchronize_rename(root_id, root, from, to, shared.cancel_flag());
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
                    return index.synchronize_rename(
                        root_id,
                        root,
                        &from,
                        to,
                        shared.cancel_flag(),
                    );
                }
            }
        }
        EventKind::Access(_) => return Ok(()),
        _ => {}
    }
    for path in event.paths {
        index.synchronize_path(root_id, root, &path, shared.cancel_flag())?;
    }
    Ok(())
}

/// Reads SQLite's `user_version` from raw header bytes (offset 60) without
/// opening the database, so refusing an unknown schema cannot mutate
/// anything — not even WAL sidecars or a hot-journal rollback, both of
/// which a `Connection::open` performs. Returns `None` when the file is
/// missing or too short to hold a header, or when the magic is wrong;
/// all three fall through to the normal open path below, which handles
/// creation and corrupt files exactly as before.
fn pre_open_user_version(path: &Path) -> Option<i64> {
    use std::io::Read as _;
    let mut file = fs::File::open(path).ok()?;
    let mut header = [0u8; 100];
    file.read_exact(&mut header).ok()?;
    if header[0..16] != *b"SQLite format 3\0" {
        return None;
    }
    Some(i64::from(u32::from_be_bytes([
        header[60], header[61], header[62], header[63],
    ])))
}

impl OwnedIndex {
    pub fn open(path: &Path) -> Result<Self, OwnedIndexError> {
        // Refuse unknown schemas before opening at all: even opening can
        // create -shm sidecars or roll back a hot journal.
        if let Some(version) = pre_open_user_version(path) {
            if version != 0 && version != 1 && version != SCHEMA_VERSION {
                return Err(OwnedIndexError::UnsupportedSchema(version));
            }
        }
        // The parent must exist before SQLite can open inside it; creating
        // it is the only filesystem effect before the version check below.
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| OwnedIndexError::Permissions {
                path: parent.display().to_string(),
                source,
            })?;
        }

        let mut connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;

        // The version gates every side effect: refusing an unknown schema
        // must leave the database byte-identical — no chmod, no WAL flip,
        // not even securing the parent — so a newer app's index still
        // opens once upgraded again.
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != 0 && version != 1 && version != SCHEMA_VERSION {
            return Err(OwnedIndexError::UnsupportedSchema(version));
        }

        if let Some(parent) = path.parent() {
            secure_directory(parent)?;
        }
        secure_file(path)?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;\n\
             PRAGMA synchronous = NORMAL;",
        )?;
        // Retried, not batched: the WAL flip bypasses the busy timeout.
        set_wal_mode(&connection)?;

        if version == SCHEMA_VERSION {
            return Ok(Self {
                connection,
                path: path.to_path_buf(),
                opened_version: SCHEMA_VERSION,
            });
        }
        // Creating or migrating under an immediate transaction: the first
        // opener wins the write lock, and a concurrent first-opener waits
        // out the busy timeout, then re-reads the version inside its own
        // transaction and finds nothing left to do. A deferred transaction
        // would let both select the migration before either locked, and the
        // loser would die creating a table the winner already committed.
        let transaction = write_transaction(&mut connection)?;
        let version: i64 =
            transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                transaction.execute_batch(SCHEMA)?;
                transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            1 => {
                transaction.execute_batch(MIGRATION_1_TO_2)?;
                transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
            // A concurrent first-opener migrated while this one waited for
            // the write lock: nothing left to do.
            SCHEMA_VERSION => {}
            other => return Err(OwnedIndexError::UnsupportedSchema(other)),
        }
        transaction.commit()?;

        Ok(Self {
            connection,
            path: path.to_path_buf(),
            opened_version: SCHEMA_VERSION,
        })
    }

    /// Opens an existing index for reading without changing the index
    /// itself: no directories are created, no permissions or journal
    /// settings are touched, and older schemas are never migrated — a
    /// version 1 database stays readable by a version 1 app. SQLite may
    /// still create `-wal`/`-shm` sidecar files beside the database while
    /// reading; the promise is about the index content, not the directory
    /// listing. `PRAGMA query_only` makes any accidental write fail loudly
    /// instead of slipping through. Readers must tolerate whatever the
    /// accepted schemas lack (today: no `daemon_state` table on version 1,
    /// and an empty file on version 0). The 5s busy timeout matches
    /// writable opens: on a read-only handle it only waits out a writer's
    /// lock, never writes.
    pub fn open_read_only(path: &Path) -> Result<Self, OwnedIndexError> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA query_only = ON;")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 | 1 | SCHEMA_VERSION => {}
            other => return Err(OwnedIndexError::UnsupportedSchema(other)),
        }
        Ok(Self {
            connection,
            path: path.to_path_buf(),
            opened_version: version,
        })
    }

    /// Whether this handle opened an empty (version 0) database: no tables
    /// exist, so every read below answers empty instead of failing.
    fn is_empty_database(&self) -> bool {
        self.opened_version == 0
    }

    /// Whether `scan_runs` has the version 2 indexable counter. A
    /// migrating open always lands on version 2; only a read-only open of
    /// a version 1 database answers false.
    fn has_indexable_column(&self) -> bool {
        self.opened_version >= SCHEMA_VERSION
    }

    pub(crate) fn roots(&self) -> Result<Vec<(String, PathBuf)>, OwnedIndexError> {
        if self.is_empty_database() {
            return Ok(Vec::new());
        }
        let mut statement = self
            .connection
            .prepare("SELECT id, path FROM roots ORDER BY id")?;
        let rows =
            statement.query_map([], |row| Ok((row.get(0)?, path_from_bytes(row.get(1)?))))?;
        rows.collect::<Result<_, _>>()
            .map_err(OwnedIndexError::from)
    }

    pub(crate) fn roots_with_state(
        &self,
    ) -> Result<Vec<(String, PathBuf, String)>, OwnedIndexError> {
        if self.is_empty_database() {
            return Ok(Vec::new());
        }
        let mut statement = self
            .connection
            .prepare("SELECT id, path, state FROM roots ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get(0)?,
                path_from_bytes(row.get::<_, Vec<u8>>(1)?),
                row.get(2)?,
            ))
        })?;
        rows.collect::<Result<_, _>>()
            .map_err(OwnedIndexError::from)
    }

    /// The recorded mount columns for a root, defaulting to empty when the
    /// row is missing.
    pub(crate) fn mount_columns(&self, id: &str) -> Result<StoredMount, OwnedIndexError> {
        self.connection
            .query_row(
                "SELECT mount_target, mount_source, mount_uuid FROM roots WHERE id = ?1",
                [id],
                |row| {
                    Ok(StoredMount {
                        target: row.get::<_, Option<Vec<u8>>>(0)?.map(path_from_bytes),
                        source: row.get(1)?,
                        uuid: row.get(2)?,
                    })
                },
            )
            .optional()
            .map(|row| row.unwrap_or_default())
            .map_err(OwnedIndexError::from)
    }

    pub(crate) fn record_mount(
        &mut self,
        id: &str,
        target: Option<&Path>,
        source: Option<&str>,
        uuid: Option<&str>,
    ) -> Result<(), OwnedIndexError> {
        let target = target.map(path_bytes);
        self.connection.execute(
            "UPDATE roots SET mount_target = ?2, mount_source = ?3, mount_uuid = ?4,
                updated_at_ns = ?5 WHERE id = ?1",
            params![id, target, source, uuid, now_ns()],
        )?;
        Ok(())
    }

    pub(crate) fn set_root_state(&mut self, id: &str, state: &str) -> Result<(), OwnedIndexError> {
        self.connection.execute(
            "UPDATE roots SET state = ?2, updated_at_ns = ?3 WHERE id = ?1",
            params![id, state, now_ns()],
        )?;
        Ok(())
    }

    pub(crate) fn search_candidates(
        &self,
        terms: &[String],
    ) -> Result<Vec<PathBuf>, OwnedIndexError> {
        if self.is_empty_database() {
            return Ok(Vec::new());
        }
        if terms.is_empty()
            || terms
                .iter()
                .any(|term| term == "*" || !is_fts_candidate_term(term))
        {
            let mut statement = self.connection.prepare(
                "SELECT DISTINCT files.path
                 FROM files
                 JOIN root_files ON root_files.file_id = files.id
                 ORDER BY files.path",
            )?;
            let rows = statement.query_map([], |row| Ok(path_from_bytes(row.get(0)?)))?;
            return rows
                .collect::<Result<_, _>>()
                .map_err(OwnedIndexError::from);
        }

        let match_query = terms
            .iter()
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT files.path
             FROM search_docs
             JOIN files ON files.id = search_docs.rowid
             JOIN root_files ON root_files.file_id = files.id
             WHERE search_docs MATCH ?1
             ORDER BY files.path",
        )?;
        let rows = statement.query_map([match_query], |row| Ok(path_from_bytes(row.get(0)?)))?;
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

    fn insert_root(&mut self, id: &str, root: &Path) -> Result<(), OwnedIndexError> {
        let transaction = write_transaction(&mut self.connection)?;
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

    /// Moves a roots row recorded under a non-canonical spelling — and the
    /// file rows scanned under it — to the canonical spelling, so lookups
    /// and storage share one path identity. One transaction: the root row
    /// and its files move together or not at all. A file row another root
    /// still references keeps its spelling (that root still matches it);
    /// the next scan prunes whatever duplicates remain. When another root
    /// already holds the canonical spelling, the write is skipped and the
    /// caller keeps matching transiently on the healed spelling. The root
    /// update is conditional on the recorded spelling: a row a concurrent
    /// rename re-pointed after we read it keeps its newer spelling.
    fn migrate_root_spelling(
        &mut self,
        root_id: &str,
        recorded: &Path,
        canonical: &Path,
    ) -> Result<(), OwnedIndexError> {
        // The file list and generation must be read under the write
        // lock, or a concurrent scan can slip rows in between. One-time
        // per legacy root, so no chunking; a very large legacy root holds
        // the lock until the migration commits, bounded by the
        // connection's busy_timeout.
        let transaction = write_transaction(&mut self.connection)?;
        if another_root_holds(&transaction, root_id, canonical)? {
            return Ok(());
        }
        let files: Vec<(i64, PathBuf)> = {
            let mut statement = transaction.prepare(
                "SELECT files.id, files.path
                 FROM files
                 JOIN root_files ON root_files.file_id = files.id
                 WHERE root_files.root_id = ?1",
            )?;
            let rows = statement.query_map([root_id], |row| {
                Ok((row.get::<_, i64>(0)?, path_from_bytes(row.get(1)?)))
            })?;
            rows.collect::<Result<_, _>>()?
        };
        let generation: i64 = transaction.query_row(
            "SELECT scan_generation FROM roots WHERE id = ?1",
            [root_id],
            |row| row.get(0),
        )?;
        // The recorded spelling was read before this transaction; a
        // concurrent rename may have re-pointed the row since. The root
        // update only lands when the row still holds what we read —
        // otherwise the newer spelling wins, the transaction rolls back
        // uncommitted, and the next sync heals whatever is still stale.
        let changed = transaction.execute(
            "UPDATE roots SET path = ?2, display_path = ?3, updated_at_ns = ?4
             WHERE id = ?1 AND path = ?5",
            params![
                root_id,
                path_bytes(canonical),
                canonical.to_string_lossy(),
                now_ns(),
                path_bytes(recorded),
            ],
        )?;
        if changed == 0 {
            return Ok(());
        }
        for (file_id, old_path) in files {
            let Ok(relative) = old_path.strip_prefix(recorded) else {
                continue;
            };
            let new_path = canonical.join(relative);
            if new_path == old_path {
                continue;
            }
            // Shared with another root, which still matches the old
            // spelling: leave it for the next scan to reconcile.
            let links: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM root_files WHERE file_id = ?1",
                [file_id],
                |row| row.get(0),
            )?;
            if links > 1 {
                continue;
            }
            let new_bytes = path_bytes(&new_path);
            let existing: Option<i64> = transaction
                .query_row(
                    "SELECT id FROM files WHERE path = ?1",
                    [&new_bytes],
                    |row| row.get(0),
                )
                .optional()?;
            match existing {
                // A canonical sync already stored the twin: move this
                // root's link onto it and drop the orphaned spelling.
                Some(canonical_id) => {
                    transaction.execute(
                        "INSERT INTO root_files (root_id, file_id, scan_generation)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(root_id, file_id) DO UPDATE SET
                            scan_generation = excluded.scan_generation",
                        params![root_id, canonical_id, generation],
                    )?;
                    transaction.execute(
                        "DELETE FROM root_files WHERE root_id = ?1 AND file_id = ?2",
                        params![root_id, file_id],
                    )?;
                    transaction.execute(
                        "DELETE FROM files
                         WHERE id = ?1 AND NOT EXISTS (
                            SELECT 1 FROM root_files WHERE root_files.file_id = files.id
                         )",
                        [file_id],
                    )?;
                }
                None => {
                    transaction.execute(
                        "UPDATE files SET path = ?2, display_path = ?3, folders_search = ?4
                         WHERE id = ?1",
                        params![
                            file_id,
                            new_bytes,
                            new_path.to_string_lossy(),
                            folders_search_text(&new_path),
                        ],
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

    fn remove_root(&mut self, id: &str) -> Result<(), OwnedIndexError> {
        let transaction = write_transaction(&mut self.connection)?;
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

    pub(crate) fn revision(&self) -> Result<u64, OwnedIndexError> {
        if self.is_empty_database() {
            return Ok(0);
        }
        let revision: i64 = self.connection.query_row(
            "SELECT value FROM schema_meta WHERE key = 'index_revision'",
            [],
            |row| row.get(0),
        )?;
        Ok(revision.max(0) as u64)
    }

    fn media_counts(&self) -> Result<(u64, u64), OwnedIndexError> {
        if self.is_empty_database() {
            return Ok((0, 0));
        }
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

    fn root_media_counts(&self, root_id: &str) -> Result<(u64, u64), OwnedIndexError> {
        if self.is_empty_database() {
            return Ok((0, 0));
        }
        let (videos, images): (i64, i64) = self.connection.query_row(
            "SELECT
                COALESCE(SUM(files.media_type = 'video'), 0),
                COALESCE(SUM(files.media_type = 'image'), 0)
             FROM root_files JOIN files ON files.id = root_files.file_id
             WHERE root_files.root_id = ?1",
            [root_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((videos as u64, images as u64))
    }

    /// Scans one root to completion. Watchers run the cancellable batch
    /// form below instead, so `stop` aborts a scan in progress.
    pub fn scan_root<F>(
        &mut self,
        root_id: &str,
        root: &Path,
        on_progress: F,
    ) -> Result<ScanProgress, OwnedIndexError>
    where
        F: FnMut(&ScanProgress),
    {
        self.scan_root_in_batches(
            root_id,
            root,
            SCAN_BATCH_SIZE,
            &AtomicBool::new(false),
            on_progress,
        )
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
        self.scan_root_in_batches(
            root_id,
            root,
            batch_size.max(1),
            &AtomicBool::new(false),
            on_progress,
        )
    }

    fn scan_root_in_batches<F>(
        &mut self,
        root_id: &str,
        root: &Path,
        batch_size: usize,
        cancel: &AtomicBool,
        mut on_progress: F,
    ) -> Result<ScanProgress, OwnedIndexError>
    where
        F: FnMut(&ScanProgress),
    {
        // A stale watcher can hand a pre-migration spelling back in: walk
        // and store the canonical spelling so a repair scan never reverts
        // a migrated roots row — unless another root already holds the
        // canonical spelling (an aliased twin the migration preserves).
        // Then `begin_scan` would fail UNIQUE on the roots row, so the
        // scan keeps the as-recorded spelling and stays usable.
        let canonical = canonical_spelling(root);
        let scan_spelling =
            if canonical != *root && another_root_holds(&self.connection, root_id, &canonical)? {
                root.to_path_buf()
            } else {
                canonical
            };
        let root = &scan_spelling;
        let (generation, mut progress) = self.begin_scan(root_id, root)?;
        on_progress(&progress);
        let mut pending = VecDeque::from([root.to_path_buf()]);
        let mut batch = Vec::with_capacity(batch_size);

        while let Some(directory) = pending.pop_front() {
            if cancel.load(Ordering::Relaxed) {
                return self.finish_cancelled_scan(&mut progress, root);
            }
            match index_visibility::is_hidden(&directory) {
                Ok(true) => {
                    progress.skipped_paths = progress.skipped_paths.saturating_add(1);
                    continue;
                }
                Ok(false) => {}
                Err(_) => {
                    progress.read_failures = progress.read_failures.saturating_add(1);
                    continue;
                }
            }
            progress.current_folder = Some(directory.clone());
            progress.current_file = None;
            progress.touch();
            // Persist here, not just at media batch commits: small trees may
            // never fill a batch, and directories without supported media
            // never commit one, so without this the reported directory would
            // stay null or show a previous directory.
            self.save_progress(&progress)?;
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
                // A directory can hold thousands of files, so each one is a
                // cancellation point too: stopping a root never waits out a
                // whole directory listing.
                if cancel.load(Ordering::Relaxed) {
                    return self.finish_cancelled_scan(&mut progress, root);
                }
                let path = entry.path();
                match index_visibility::is_hidden(&path) {
                    Ok(true) => {
                        progress.skipped_paths = progress.skipped_paths.saturating_add(1);
                        continue;
                    }
                    Ok(false) => {}
                    Err(_) => {
                        progress.read_failures = progress.read_failures.saturating_add(1);
                        continue;
                    }
                }
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
                // Indexable only: unsupported, empty, and unreadable files
                // never reach the batch, so the queued-work count built
                // from this counter does not mistake them for remaining
                // work.
                progress.files_indexable = progress.files_indexable.saturating_add(1);
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
        let transaction = write_transaction(&mut self.connection)?;
        // Update-only: a scan must never resurrect a root that was removed
        // after the caller read the roots table.
        let changed = transaction.execute(
            "UPDATE roots SET
                path = ?2, display_path = ?3, state = 'scanning',
                scan_generation = scan_generation + 1,
                updated_at_ns = ?4
             WHERE id = ?1",
            params![
                root_id,
                root_bytes,
                root.to_string_lossy(),
                progress.started_at_ns
            ],
        )?;
        if changed == 0 {
            return Err(OwnedIndexError::FolderNotConfigured);
        }
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
        let transaction = write_transaction(&mut self.connection)?;
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
        fire_sync_probe(root_id, SyncProbeSite::ScanBatchCommitted);
        Ok(())
    }

    fn save_progress(&mut self, progress: &ScanProgress) -> Result<(), OwnedIndexError> {
        let transaction = write_transaction(&mut self.connection)?;
        persist_progress(&transaction, progress)?;
        transaction.commit()?;
        Ok(())
    }

    /// Aborts a scan without recording it as finished: the roots row and
    /// the scan run stay `scanning` with partial counts, committed batches
    /// keep their generation, and the next scan supersedes all of it — an
    /// incomplete scan is never trusted as successful.
    fn finish_cancelled_scan(
        &mut self,
        progress: &mut ScanProgress,
        root: &Path,
    ) -> Result<ScanProgress, OwnedIndexError> {
        progress.touch();
        self.save_progress(progress)?;
        Err(OwnedIndexError::Cancelled {
            path: root.display().to_string(),
        })
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
        let transaction = write_transaction(&mut self.connection)?;
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
        let transaction = write_transaction(&mut self.connection)?;
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
        if self.is_empty_database() {
            return Ok(None);
        }
        // Version 1 has no `files_indexable` column; the committed count
        // stands in for it there, so remaining work reads as none rather
        // than inventing a backlog. Selecting it in the same position
        // keeps every column index below identical on both schemas.
        let indexable = if self.has_indexable_column() {
            "files_indexable"
        } else {
            "files_indexed"
        };
        let query = format!(
            "SELECT root_id, state,
                folders_discovered, folders_scanned,
                files_discovered, {indexable}, files_indexed,
                videos_indexed, images_indexed,
                skipped_paths, read_failures,
                current_folder, current_file,
                started_at_ns, updated_at_ns, error
             FROM scan_runs
             WHERE root_id = ?1
             ORDER BY generation DESC
             LIMIT 1"
        );
        self.connection
            .query_row(&query, [root_id], |row| {
                let phase = ScanPhase::from_database(&row.get::<_, String>(1)?)?;
                let started_at_ns: i64 = row.get(13)?;
                let updated_at_ns: i64 = row.get(14)?;
                let files_indexed = row.get::<_, i64>(6)? as u64;
                let elapsed_ns = updated_at_ns.saturating_sub(started_at_ns).max(1);
                Ok(ScanProgress {
                    root_id: row.get(0)?,
                    phase,
                    folders_discovered: row.get::<_, i64>(2)? as u64,
                    folders_scanned: row.get::<_, i64>(3)? as u64,
                    files_discovered: row.get::<_, i64>(4)? as u64,
                    files_indexable: row.get::<_, i64>(5)? as u64,
                    files_indexed,
                    videos_indexed: row.get::<_, i64>(7)? as u64,
                    images_indexed: row.get::<_, i64>(8)? as u64,
                    skipped_paths: row.get::<_, i64>(9)? as u64,
                    read_failures: row.get::<_, i64>(10)? as u64,
                    current_folder: row.get::<_, Option<Vec<u8>>>(11)?.map(path_from_bytes),
                    current_file: row.get::<_, Option<Vec<u8>>>(12)?.map(path_from_bytes),
                    started_at_ns,
                    updated_at_ns,
                    scan_rate: files_indexed as f64 * 1_000_000_000.0 / elapsed_ns as f64,
                    error: row.get(15)?,
                })
            })
            .optional()
            .map_err(OwnedIndexError::from)
    }

    /// Replaces the single daemon liveness row with this heartbeat. The
    /// daemon is the only writer; `ts status` only reads.
    pub(crate) fn publish_daemon_state(
        &self,
        pid: u32,
        started_at_ns: i64,
        beat: &crate::daemon_state::DaemonBeat,
    ) -> Result<(), OwnedIndexError> {
        self.connection.execute(
            "INSERT INTO daemon_state (
                id, pid, started_at_ns, heartbeat_at_ns, activity,
                current_root, current_path,
                pending_roots, files_remaining, pending_repairs, pending_thumbnails
             ) VALUES (
                1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
             )
             ON CONFLICT(id) DO UPDATE SET
                pid = excluded.pid,
                started_at_ns = excluded.started_at_ns,
                heartbeat_at_ns = excluded.heartbeat_at_ns,
                activity = excluded.activity,
                current_root = excluded.current_root,
                current_path = excluded.current_path,
                pending_roots = excluded.pending_roots,
                files_remaining = excluded.files_remaining,
                pending_repairs = excluded.pending_repairs,
                pending_thumbnails = excluded.pending_thumbnails",
            params![
                i64::from(pid),
                started_at_ns,
                beat.heartbeat_at_ns,
                beat.activity.as_database(),
                beat.current_root.as_deref().map(path_bytes),
                beat.current_path.as_deref().map(path_bytes),
                database_count(beat.queue.pending_roots),
                database_count(beat.queue.files_remaining),
                database_count(beat.queue.pending_repairs),
                database_count(beat.queue.pending_thumbnails),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn read_daemon_state(
        &self,
    ) -> Result<Option<crate::daemon_state::DaemonStateRow>, OwnedIndexError> {
        // Version 1 predates the liveness row; a missing table reads as no
        // heartbeat rather than an error, so read-only status works there.
        let has_table: bool = self.connection.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master
             WHERE type = 'table' AND name = 'daemon_state'",
            [],
            |row| row.get(0),
        )?;
        if !has_table {
            return Ok(None);
        }
        self.connection
            .query_row(
                "SELECT pid, started_at_ns, heartbeat_at_ns, activity,
                    current_root, current_path,
                    pending_roots, files_remaining, pending_repairs, pending_thumbnails
                 FROM daemon_state
                 WHERE id = 1",
                [],
                |row| {
                    let pid: i64 = row.get(0)?;
                    Ok(crate::daemon_state::DaemonStateRow {
                        // A pid that does not fit (or zero, which signals a
                        // process group rather than a process) can never be
                        // alive, so it reads as unknown.
                        pid: u32::try_from(pid).ok().filter(|pid| *pid != 0),
                        started_at_ns: row.get(1)?,
                        heartbeat_at_ns: row.get(2)?,
                        activity: crate::daemon_state::DaemonActivity::from_database(
                            &row.get::<_, String>(3)?,
                        ),
                        current_root: row.get::<_, Option<Vec<u8>>>(4)?.map(path_from_bytes),
                        current_path: row.get::<_, Option<Vec<u8>>>(5)?.map(path_from_bytes),
                        queue: crate::daemon_state::DaemonQueue {
                            // Stored counts are trusted but tamperable; a
                            // negative reads as zero rather than wrapping
                            // to billions of phantom files.
                            pending_roots: row.get::<_, i64>(6)?.max(0) as u64,
                            files_remaining: row.get::<_, i64>(7)?.max(0) as u64,
                            pending_repairs: row.get::<_, i64>(8)?.max(0) as u64,
                            pending_thumbnails: row.get::<_, i64>(9)?.max(0) as u64,
                        },
                    })
                },
            )
            .optional()
            .map_err(OwnedIndexError::from)
    }

    /// Roots whose storage is missing, as last recorded. Read live by
    /// `ts status` (like the service state) rather than published with the
    /// heartbeat, so no schema change carries it. A missing `roots` table
    /// — an empty database — reads as none.
    pub(crate) fn offline_root_count(&self) -> Result<u64, OwnedIndexError> {
        let has_table: bool = self.connection.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master
             WHERE type = 'table' AND name = 'roots'",
            [],
            |row| row.get(0),
        )?;
        if !has_table {
            return Ok(0);
        }
        let offline: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM roots WHERE state = 'offline'",
            [],
            |row| row.get(0),
        )?;
        Ok(offline.max(0) as u64)
    }

    /// The queued work the daemon publishes with its heartbeat: roots not
    /// fully indexed, indexable files seen but uncommitted by scans in
    /// progress, and queued repairs, plus the thumbnail backlog the caller
    /// counts live.
    pub(crate) fn daemon_queue_counts(
        &self,
        pending_thumbnails: u64,
    ) -> Result<crate::daemon_state::DaemonQueue, OwnedIndexError> {
        // Pending, scanning, and errored roots: work the daemon can act
        // on. Offline roots count separately — `ts status` lists them on
        // their own line, and they never hold `ts status --watch`.
        let pending_roots: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM roots WHERE state != 'ready' AND state != 'offline'",
            [],
            |row| row.get(0),
        )?;
        // Only each root's latest run counts: an older 'scanning' row is
        // history from a generation a later scan superseded. Indexable,
        // not discovered: unsupported, empty, and unreadable files never
        // reach the batch, so counting them would report already-decided
        // files as remaining work. Each run clamps at zero before
        // summing: a migrated version 1 row (indexable 0, indexed N)
        // would otherwise contribute -N and hide another root's real
        // remainder behind the single clamp below.
        let files_remaining: Option<i64> = self.connection.query_row(
            "SELECT SUM(MAX(files_indexable - files_indexed, 0))
             FROM scan_runs
             WHERE state = 'scanning'
               AND generation = (
                    SELECT scan_generation FROM roots WHERE id = scan_runs.root_id
               )",
            [],
            |row| row.get(0),
        )?;
        let pending_repairs: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM repair_queue", [], |row| row.get(0))?;
        Ok(crate::daemon_state::DaemonQueue {
            pending_roots: pending_roots as u64,
            files_remaining: files_remaining.unwrap_or(0).max(0) as u64,
            pending_repairs: pending_repairs as u64,
            pending_thumbnails,
        })
    }

    pub(crate) fn synchronize_path(
        &mut self,
        root_id: &str,
        root: &Path,
        path: &Path,
        cancel: &AtomicBool,
    ) -> Result<(), OwnedIndexError> {
        match self.try_synchronize_path(root_id, root, path, cancel) {
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
        cancel: &AtomicBool,
    ) -> Result<(), OwnedIndexError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(OwnedIndexError::Cancelled {
                path: path.display().to_string(),
            });
        }
        if !path.starts_with(root) {
            return Ok(());
        }
        if let Some(hidden) = index_visibility::hidden_ancestor(root, path).map_err(|source| {
            OwnedIndexError::PathRead {
                path: path.display().to_string(),
                source,
            }
        })? {
            return self.remove_indexed_path(root_id, hidden);
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
            return self.synchronize_tree(root_id, root, path, cancel);
        }
        let Some(file) = metadata
            .filter(fs::Metadata::is_file)
            .and_then(|metadata| IndexedFile::read(path.to_path_buf(), metadata))
        else {
            return self.remove_indexed_path(root_id, path);
        };
        let transaction = write_transaction(&mut self.connection)?;
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
        fire_sync_probe(root_id, SyncProbeSite::FileCommitted);
        Ok(())
    }

    fn synchronize_tree(
        &mut self,
        root_id: &str,
        root: &Path,
        tree: &Path,
        cancel: &AtomicBool,
    ) -> Result<(), OwnedIndexError> {
        let mut pending = VecDeque::from([tree.to_path_buf()]);
        while let Some(directory) = pending.pop_front() {
            if cancel.load(Ordering::Relaxed) {
                return Err(OwnedIndexError::Cancelled {
                    path: directory.display().to_string(),
                });
            }
            let entries = fs::read_dir(&directory).map_err(|source| OwnedIndexError::PathRead {
                path: directory.display().to_string(),
                source,
            })?;
            for entry in entries {
                // Each file commits its own transaction, so each one is a
                // cancellation point too: stopping a root never waits out
                // a whole queued directory, and the repair stays queued.
                if cancel.load(Ordering::Relaxed) {
                    return Err(OwnedIndexError::Cancelled {
                        path: directory.display().to_string(),
                    });
                }
                let entry = entry.map_err(|source| OwnedIndexError::PathRead {
                    path: directory.display().to_string(),
                    source,
                })?;
                let path = entry.path();
                if index_visibility::is_hidden(&path).map_err(|source| {
                    OwnedIndexError::PathRead {
                        path: path.display().to_string(),
                        source,
                    }
                })? {
                    self.remove_indexed_path(root_id, &path)?;
                    continue;
                }
                let file_type = entry
                    .file_type()
                    .map_err(|source| OwnedIndexError::PathRead {
                        path: path.display().to_string(),
                        source,
                    })?;
                if file_type.is_dir() {
                    pending.push_back(path);
                } else if file_type.is_file() {
                    self.try_synchronize_path(root_id, root, &path, cancel)?;
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
        let transaction = write_transaction(&mut self.connection)?;
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

    pub(crate) fn repair_pending<F>(
        &mut self,
        root_id: &str,
        root: &Path,
        cancel: &AtomicBool,
        mut on_progress: F,
    ) -> Result<usize, OwnedIndexError>
    where
        F: FnMut(&ScanProgress),
    {
        let repairs = self.pending_repairs()?;
        let mut repaired = 0;
        for repair in repairs
            .into_iter()
            .filter(|repair| repair.path.starts_with(root))
        {
            if cancel.load(Ordering::Relaxed) {
                return Err(OwnedIndexError::Cancelled {
                    path: root.display().to_string(),
                });
            }
            let result = if repair.reason == "direct rename" {
                self.scan_root_in_batches(root_id, root, SCAN_BATCH_SIZE, cancel, |progress| {
                    on_progress(progress)
                })
                .map(|_| ())
            } else {
                self.try_synchronize_path(root_id, root, &repair.path, cancel)
            };
            match result {
                Ok(()) => {
                    self.connection.execute(
                        "DELETE FROM repair_queue WHERE path = ?1",
                        [path_bytes(&repair.path)],
                    )?;
                    repaired += 1;
                }
                // A cancelled repair stays queued for the next run; only a
                // genuine failure is re-queued with its error.
                Err(error) if matches!(error, OwnedIndexError::Cancelled { .. }) => {
                    return Err(error);
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
        cancel: &AtomicBool,
    ) -> Result<(), OwnedIndexError> {
        match self.try_synchronize_rename(root_id, root, from, to, cancel) {
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
        cancel: &AtomicBool,
    ) -> Result<(), OwnedIndexError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(OwnedIndexError::Cancelled {
                path: to.display().to_string(),
            });
        }
        let root_moved = from == root;
        if !root_moved {
            match (from.starts_with(root), to.starts_with(root)) {
                (false, false) => return Ok(()),
                (true, false) => return self.remove_indexed_path(root_id, from),
                (false, true) => return self.try_synchronize_path(root_id, root, to, cancel),
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
                // Stopping mid-rename must not wait out the scan of every
                // row, its metadata reads, and the rewrite commit: each
                // planned row is a cancellation point.
                if cancel.load(Ordering::Relaxed) {
                    return Err(OwnedIndexError::Cancelled {
                        path: to.display().to_string(),
                    });
                }
                fire_sync_probe(root_id, SyncProbeSite::RenamePlanned);
                let (file_id, old_path) = row?;
                let Ok(relative) = old_path.strip_prefix(from) else {
                    continue;
                };
                let new_path = if relative.as_os_str().is_empty() {
                    to.to_path_buf()
                } else {
                    to.join(relative)
                };
                if index_visibility::hidden_ancestor(if root_moved { to } else { root }, &new_path)
                    .map_err(|source| OwnedIndexError::PathRead {
                        path: new_path.display().to_string(),
                        source,
                    })?
                    .is_some()
                {
                    moved_files.push((file_id, None));
                    continue;
                }
                let Some(file) = fs::metadata(&new_path)
                    .ok()
                    .filter(|metadata| metadata.is_file())
                    .and_then(|metadata| IndexedFile::read(new_path, metadata))
                else {
                    continue;
                };
                moved_files.push((file_id, Some(file)));
            }
            moved_files
        };
        // Planning finished after the stop arrived: do not open the
        // rewrite transaction at all.
        if cancel.load(Ordering::Relaxed) {
            return Err(OwnedIndexError::Cancelled {
                path: to.display().to_string(),
            });
        }
        let transaction = write_transaction(&mut self.connection)?;
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
            // Each rewritten row is a cancellation point too: dropping
            // the uncommitted transaction rolls everything back, so a
            // cancelled rename commits nothing — not even the roots row
            // already staged above.
            if cancel.load(Ordering::Relaxed) {
                drop(transaction);
                return Err(OwnedIndexError::Cancelled {
                    path: to.display().to_string(),
                });
            }
            fire_sync_probe(root_id, SyncProbeSite::RenameRewritten);
            let Some(file) = file else {
                transaction.execute("DELETE FROM files WHERE id = ?1", [file_id])?;
                continue;
            };
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
        // Recheck after the last row: a stop landing between the final
        // per-row check and the commit below must still roll everything
        // back — otherwise the rename commits while the destination sync
        // reports it cancelled.
        if cancel.load(Ordering::Relaxed) {
            drop(transaction);
            return Err(OwnedIndexError::Cancelled {
                path: to.display().to_string(),
            });
        }
        transaction.execute(
            "UPDATE schema_meta SET value = value + 1 WHERE key = 'index_revision'",
            [],
        )?;
        transaction.commit()?;
        self.try_synchronize_path(root_id, if root_moved { to } else { root }, to, cancel)
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
            files_indexable = ?6,
            files_indexed = ?7,
            videos_indexed = ?8,
            images_indexed = ?9,
            skipped_paths = ?10,
            read_failures = ?11,
            current_folder = ?12,
            current_file = ?13,
            updated_at_ns = ?14,
            finished_at_ns = ?15,
            error = ?16
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
            database_count(progress.files_indexable),
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

/// The folder half of a file's search document: its parent directory,
/// lossy and lowercased. Shared by indexing and root-spelling migration
/// so the two can never derive different documents for one path.
fn folders_search_text(path: &Path) -> String {
    path.parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .to_lowercase()
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

fn is_fts_candidate_term(term: &str) -> bool {
    term.len() >= 3 && term.bytes().all(|byte| byte.is_ascii_alphanumeric())
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
        let busy_timeout: i64 = index
            .connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy_timeout, 5_000);
        assert_eq!(version, 2);
        for table in [
            "schema_meta",
            "roots",
            "files",
            "root_files",
            "search_docs",
            "scan_runs",
            "repair_queue",
            "daemon_state",
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
    fn scans_skip_dot_files_and_do_not_enter_dot_folders() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        fs::create_dir_all(root.join(".worktrees/nested")).unwrap();
        fs::create_dir_all(root.join("visible.folder")).unwrap();
        fs::write(root.join(".hidden.mp4"), b"video").unwrap();
        fs::write(root.join(".worktrees/nested/clip.mp4"), b"video").unwrap();
        fs::write(root.join("visible.folder/photo.jpg"), b"photo").unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
        let mut scanned_folders = Vec::new();

        let progress = index
            .scan_root("root", &root, |progress| {
                if let Some(folder) = &progress.current_folder {
                    scanned_folders.push(folder.clone());
                }
            })
            .unwrap();

        assert_eq!(progress.files_indexed, 1);
        assert_eq!(progress.skipped_paths, 2);
        assert_eq!(progress.read_failures, 0);
        assert!(!scanned_folders.contains(&root.join(".worktrees")));
        let state = OwnedIndexManager::under(database).state().unwrap();
        assert_eq!((state.indexed_videos, state.indexed_images), (0, 1));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn rescans_remove_media_in_folders_with_the_macos_hidden_flag() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let hidden = root.join("archive");
        fs::create_dir_all(hidden.join("nested")).unwrap();
        fs::write(hidden.join("nested/clip.mp4"), b"video").unwrap();
        fs::write(root.join("photo.jpg"), b"photo").unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        assert!(std::process::Command::new("/usr/bin/chflags")
            .arg("hidden")
            .arg(&hidden)
            .status()
            .unwrap()
            .success());

        let progress = index.scan_root("root", &root, |_| {}).unwrap();

        assert_eq!(progress.files_indexed, 1);
        assert_eq!(progress.folders_scanned, 1);
        assert_eq!(progress.skipped_paths, 1);
        assert_eq!(progress.read_failures, 0);
        let manager = OwnedIndexManager::under(database);
        let state = manager.state().unwrap();
        assert_eq!((state.indexed_videos, state.indexed_images), (0, 1));
        assert!(std::process::Command::new("/usr/bin/chflags")
            .arg("nohidden")
            .arg(&hidden)
            .status()
            .unwrap()
            .success());
        index
            .synchronize_path("root", &root, &hidden, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(manager.state().unwrap().indexed_videos, 1);
        assert!(std::process::Command::new("/usr/bin/chflags")
            .arg("hidden")
            .arg(&hidden)
            .status()
            .unwrap()
            .success());
        index
            .synchronize_path(
                "root",
                &root,
                &hidden.join("nested/clip.mp4"),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(manager.state().unwrap().indexed_videos, 0);
    }

    #[test]
    fn live_updates_skip_hidden_files_and_hidden_ancestors() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        fs::create_dir(&root).unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        fs::create_dir_all(root.join("new/.worktrees/nested")).unwrap();
        fs::write(root.join("new/.worktrees/nested/clip.mp4"), b"video").unwrap();
        fs::write(root.join("new/.hidden.mp4"), b"video").unwrap();
        fs::write(root.join("new/photo.jpg"), b"photo").unwrap();

        index
            .synchronize_path("root", &root, &root.join("new"), &AtomicBool::new(false))
            .unwrap();
        index
            .synchronize_path(
                "root",
                &root,
                &root.join("new/.worktrees/nested/clip.mp4"),
                &AtomicBool::new(false),
            )
            .unwrap();
        index
            .synchronize_path(
                "root",
                &root,
                &root.join("new/.hidden.mp4"),
                &AtomicBool::new(false),
            )
            .unwrap();

        let state = OwnedIndexManager::under(database).state().unwrap();
        assert_eq!((state.indexed_videos, state.indexed_images), (0, 1));
        assert!(index.pending_repairs().unwrap().is_empty());
    }

    #[test]
    fn renaming_files_and_folders_to_hidden_names_removes_them_until_restored() {
        for (visible_name, hidden_name, file_name) in [
            ("clip.mp4", ".clip.mp4", None),
            ("archive", ".archive", Some("clip.mp4")),
        ] {
            let directory = tempdir().unwrap();
            let root = directory.path().join("media");
            fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
            let visible = root.join(visible_name);
            let hidden = root.join(hidden_name);
            let file = if let Some(name) = file_name {
                fs::create_dir(&visible).unwrap();
                visible.join(name)
            } else {
                visible.clone()
            };
            fs::write(file, b"video").unwrap();
            let database = directory.path().join("index.sqlite3");
            let mut index = OwnedIndex::open(&database).unwrap();
            index.insert_root("root", &root).unwrap();
            index.scan_root("root", &root, |_| {}).unwrap();
            let manager = OwnedIndexManager::under(database);

            fs::rename(&visible, &hidden).unwrap();
            index
                .synchronize_rename("root", &root, &visible, &hidden, &AtomicBool::new(false))
                .unwrap();
            assert_eq!(manager.state().unwrap().indexed_videos, 0, "{hidden_name}");

            fs::rename(&hidden, &visible).unwrap();
            index
                .synchronize_rename("root", &root, &hidden, &visible, &AtomicBool::new(false))
                .unwrap();
            assert_eq!(manager.state().unwrap().indexed_videos, 1, "{visible_name}");
        }
    }

    #[test]
    fn a_hidden_search_root_is_not_scanned() {
        let directory = tempdir().unwrap();
        let root = directory.path().join(".media");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("clip.mp4"), b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();

        index.insert_root("root", &root).unwrap();
        let progress = index.scan_root("root", &root, |_| {}).unwrap();

        assert_eq!(progress.files_indexed, 0);
        assert_eq!(progress.folders_scanned, 0);
        assert_eq!(progress.skipped_paths, 1);
    }

    #[test]
    fn search_candidates_use_the_full_text_index_and_widen_when_needed() {
        let (directory, mut index) = open_index();
        let root = directory.path().join("media");
        let nested = root.join("Sample Set");
        std::fs::create_dir_all(&nested).unwrap();
        let root = root.canonicalize().unwrap();
        let nested = root.join("Sample Set");
        let sample = root.join("sample.mp4");
        let clip = nested.join("clip.mov");
        let other = root.join("other.mp4");
        for path in [&sample, &clip, &other] {
            std::fs::write(path, b"media").unwrap();
        }
        index.insert_root("media", &root).unwrap();
        index.scan_root("media", &root, |_| {}).unwrap();

        let mut sample_candidates = index.search_candidates(&["sample".into()]).unwrap();
        sample_candidates.sort();
        let mut sample_matches = vec![sample.clone(), clip.clone()];
        sample_matches.sort();
        assert_eq!(sample_candidates, sample_matches);

        let mut all_candidates = vec![sample.clone(), clip.clone(), other.clone()];
        all_candidates.sort();
        let mut multi_term_candidates = index
            .search_candidates(&["sample".into(), "other".into()])
            .unwrap();
        multi_term_candidates.sort();
        assert_eq!(multi_term_candidates, all_candidates);

        let mut short_term_candidates = index.search_candidates(&["sa".into()]).unwrap();
        short_term_candidates.sort();
        assert_eq!(short_term_candidates, all_candidates);
        assert_eq!(
            index.search_candidates(&["*".into()]).unwrap(),
            all_candidates
        );
    }

    #[test]
    fn an_initial_scan_indexes_media_and_reports_detailed_progress() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let nested = root.join("Family Archive");
        std::fs::create_dir_all(&nested).unwrap();
        let root = root.canonicalize().unwrap();
        let nested = root.join("Family Archive");
        std::fs::write(nested.join("Trip [home holiday].JPG"), b"photo").unwrap();
        std::fs::write(nested.join("Clip.mp4"), b"video").unwrap();
        std::fs::write(nested.join("notes.txt"), b"text").unwrap();
        std::fs::File::create(nested.join("empty.mov")).unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
        let mut updates = Vec::new();

        let finished = index
            .scan_root("root", &root, |progress| updates.push(progress.clone()))
            .unwrap();

        assert_eq!(finished.phase, ScanPhase::Ready);
        assert_eq!(finished.folders_discovered, 2);
        assert_eq!(finished.folders_scanned, 2);
        assert_eq!(finished.files_discovered, 4);
        assert_eq!(finished.files_indexable, 2);
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
        assert_eq!(stored.files_indexable, 2);
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
        scanner.insert_root("root", &root).unwrap();
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
        let root = root.canonicalize().unwrap();
        let kept = root.join("kept.mp4");
        let removed = root.join("removed.mp4");
        std::fs::write(&kept, b"keep").unwrap();
        std::fs::write(&removed, b"remove").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
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
        let root = root.canonicalize().unwrap();
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"clip").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
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

        index.insert_root("parent", &parent).unwrap();
        index.scan_root("parent", &parent, |_| {}).unwrap();
        index.insert_root("child", &child).unwrap();
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
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("clip.mp4"), b"clip").unwrap();
        let database = root.join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();

        index.insert_root("root", &root).unwrap();
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

    /// Records `path` as indexed under `root_id`, as a pre-canonical scan
    /// would have left it: one files row plus its root_files link. A real
    /// scan cannot build this state — it heals the roots row it walks —
    /// so legacy-row tests insert it directly.
    fn index_legacy_file_row(index: &OwnedIndex, root_id: &str, path: &Path) {
        let metadata = std::fs::metadata(path).unwrap();
        let file = IndexedFile::read(path.to_path_buf(), metadata).unwrap();
        index
            .connection
            .execute(
                "INSERT INTO files (
                    path, display_path, file_name, extension, media_type,
                    size_bytes, created_at_ns, modified_at_ns,
                    file_name_search, tags_search, folders_search
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
            )
            .unwrap();
        let file_id: i64 = index
            .connection
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                [path_bytes(path)],
                |row| row.get(0),
            )
            .unwrap();
        index
            .connection
            .execute(
                "INSERT INTO root_files (root_id, file_id, scan_generation)
                 VALUES (?1, ?2, 0)",
                params![root_id, file_id],
            )
            .unwrap();
    }

    /// Every row a migration could rewrite, plus the revision: an unmatched
    /// sync must leave all of it byte-identical. `quote` renders each value
    /// in its storage form, so blobs, nulls, and timestamps compare exactly.
    fn dump_index_state(index: &OwnedIndex) -> Vec<String> {
        let dump_table = |sql: &str| -> Vec<String> {
            index
                .connection
                .prepare(sql)
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let mut dump = vec![format!("revision {}", index.revision().unwrap())];
        dump.extend(dump_table(
            "SELECT 'meta ' || quote(key) || ' ' || quote(value)
             FROM schema_meta ORDER BY key",
        ));
        dump.extend(dump_table(
            "SELECT 'root ' || quote(id) || ' ' || quote(path) || ' '
                || quote(display_path) || ' ' || quote(mount_target) || ' '
                || quote(mount_source) || ' ' || quote(mount_uuid) || ' '
                || quote(state) || ' ' || quote(scan_generation) || ' '
                || quote(created_at_ns) || ' ' || quote(updated_at_ns)
             FROM roots ORDER BY id",
        ));
        dump.extend(dump_table(
            "SELECT 'file ' || quote(id) || ' ' || quote(path) || ' '
                || quote(display_path) || ' ' || quote(file_name) || ' '
                || quote(extension) || ' ' || quote(media_type) || ' '
                || quote(size_bytes) || ' ' || quote(created_at_ns) || ' '
                || quote(modified_at_ns) || ' ' || quote(file_name_search) || ' '
                || quote(tags_search) || ' ' || quote(folders_search)
             FROM files ORDER BY id",
        ));
        dump.extend(dump_table(
            "SELECT 'link ' || quote(root_id) || ' ' || quote(file_id) || ' '
                || quote(scan_generation)
             FROM root_files ORDER BY root_id, file_id",
        ));
        dump
    }

    #[test]
    fn app_mutations_update_the_owned_index_before_returning() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        // Canonical, as production roots always are (`add_folder` stores
        // nothing else): the sync layer matches on canonical spellings.
        let root = root.canonicalize().unwrap();
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
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

    /// Serializes the tests that move the process working directory: the
    /// switch itself is process-wide, so two of them must never overlap.
    static WORKING_DIRECTORY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs a closure with the process working directory changed, restoring
    /// it afterwards. Hold `WORKING_DIRECTORY_LOCK` while the guard lives.
    struct WorkingDirectoryGuard {
        previous: PathBuf,
    }

    impl WorkingDirectoryGuard {
        fn enter(directory: &Path) -> Self {
            let previous = std::env::current_dir().unwrap();
            std::env::set_current_dir(directory).unwrap();
            Self { previous }
        }
    }

    impl Drop for WorkingDirectoryGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous);
        }
    }

    #[test]
    fn canonical_spelling_cleans_existing_paths() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let file = root.join("clip.mp4");
        std::fs::write(&file, b"video").unwrap();
        let canonical = file.canonicalize().unwrap();

        assert_eq!(canonical_spelling(&file), canonical);
        assert_eq!(
            canonical_spelling(&root.join("sub").join("..").join("clip.mp4")),
            canonical
        );
        assert_eq!(
            canonical_spelling(&root.join(".").join("clip.mp4")),
            canonical
        );
    }

    #[test]
    fn canonical_spelling_rejoins_missing_paths_onto_existing_ancestors() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();

        // A deleted file keeps the spelling its directory gives it.
        assert_eq!(
            canonical_spelling(&root.join("gone.mp4")),
            root.join("gone.mp4")
        );
        // So does a path nested under missing directories.
        assert_eq!(
            canonical_spelling(&root.join("a").join("b.mp4")),
            root.join("a").join("b.mp4")
        );
        // And a `..` past a missing part still climbs out of it.
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        assert_eq!(
            canonical_spelling(&root.join("gone").join("..").join("clip.mp4")),
            clip
        );
    }

    #[test]
    fn canonical_spelling_resolves_relative_paths_against_the_working_directory() {
        let _lock = WORKING_DIRECTORY_LOCK.lock().unwrap();
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        let _guard = WorkingDirectoryGuard::enter(&root);
        let cwd = root.canonicalize().unwrap();

        assert_eq!(
            canonical_spelling(Path::new("clip.mp4")),
            cwd.join("clip.mp4")
        );
        // The shell spelling that added a now-deleted folder still
        // resolves to where the recorded entry points.
        assert_eq!(
            canonical_spelling(Path::new("gone.mp4")),
            cwd.join("gone.mp4")
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_spelling_resolves_symlinked_ancestors() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();

        assert_eq!(
            canonical_spelling(&link.join("clip.mp4")),
            clip.canonicalize().unwrap()
        );
    }

    #[test]
    fn canonical_spelling_leaves_empty_paths_alone() {
        // Fail closed: resolving "" against the working directory would
        // silently retarget the call at whatever the shell stands in.
        assert_eq!(canonical_spelling(Path::new("")), PathBuf::from(""));
    }

    #[cfg(unix)]
    #[test]
    fn canonical_spelling_resolves_symlinks_hidden_behind_missing_parts() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let canonical_real = root.join("real").canonicalize().unwrap();
        // `gone` is missing, so the first walk rebuilds through the link;
        // the link itself must still resolve, or the sync stores an alias
        // row beside the indexed canonical one.
        let hidden = root.join("gone").join("..").join("link").join("clip.mp4");

        // The deleted-file spelling, with no endpoint on disk.
        assert_eq!(canonical_spelling(&hidden), canonical_real.join("clip.mp4"));
        // Same when the file exists through the link.
        std::fs::write(root.join("real").join("clip.mp4"), b"video").unwrap();
        assert_eq!(canonical_spelling(&hidden), canonical_real.join("clip.mp4"));
    }

    #[cfg(unix)]
    #[test]
    fn canonical_spelling_preserves_non_utf8_bytes_in_missing_paths() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        // Missing on purpose: only the rejoin runs, no filesystem name.
        let missing = root.join(OsString::from_vec(b"vid\xe9name.mp4".to_vec()));

        let spelling = canonical_spelling(&missing);

        assert_eq!(
            spelling.as_os_str().as_bytes(),
            missing.as_os_str().as_bytes()
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_spelling_resolves_nested_symlinks() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("clip.mp4"), b"video").unwrap();
        let inner = directory.path().join("inner");
        let outer = directory.path().join("outer");
        std::os::unix::fs::symlink(&real, &inner).unwrap();
        std::os::unix::fs::symlink(&inner, &outer).unwrap();
        let canonical_real = real.canonicalize().unwrap();

        // Through two links, existing and missing alike.
        assert_eq!(
            canonical_spelling(&outer.join("clip.mp4")),
            canonical_real.join("clip.mp4")
        );
        assert_eq!(
            canonical_spelling(&outer.join("gone.mp4")),
            canonical_real.join("gone.mp4")
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_spelling_terminates_on_symlink_loops() {
        let directory = tempdir().unwrap();
        let link_a = directory.path().join("link-a");
        let link_b = directory.path().join("link-b");
        std::os::unix::fs::symlink(&link_b, &link_a).unwrap();
        std::os::unix::fs::symlink(&link_a, &link_b).unwrap();
        let parent = directory.path().canonicalize().unwrap();

        // Unresolvable, so the walk rejoins onto the nearest existing
        // ancestor; what matters is that it returns instead of hanging.
        assert_eq!(
            canonical_spelling(&link_a.join("clip.mp4")),
            parent.join("link-a").join("clip.mp4")
        );
        assert_eq!(canonical_spelling(&link_a), parent.join("link-a"));
    }

    #[cfg(unix)]
    #[test]
    fn canonical_spelling_falls_back_when_the_working_directory_is_unreadable() {
        let _lock = WORKING_DIRECTORY_LOCK.lock().unwrap();
        let directory = tempdir().unwrap();
        let cwd = directory.path().join("cwd");
        std::fs::create_dir(&cwd).unwrap();
        let _guard = WorkingDirectoryGuard::enter(&cwd);
        // Deleted out from under the process: `current_dir` now fails, so
        // relative inputs cannot resolve against it.
        std::fs::remove_dir(&cwd).unwrap();
        assert!(std::env::current_dir().is_err());

        // Lexically cleaned but still relative: no silent retarget at
        // whatever directory happens to exist.
        assert_eq!(
            canonical_spelling(Path::new("foo/../bar.mp4")),
            PathBuf::from("bar.mp4")
        );
        assert_eq!(
            canonical_spelling(Path::new("clip.mp4")),
            PathBuf::from("clip.mp4")
        );
        assert_eq!(canonical_spelling(Path::new("")), PathBuf::from(""));
    }

    fn scanned_canonical_root(
        directory: &Path,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, OwnedIndexUpdates) {
        // Production roots are always canonical (`add_folder` stores
        // nothing else), so the sync tests record canonical roots too.
        let scratch = tempdir().unwrap();
        let root = scratch.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let database = directory.join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        drop(index);
        let updates = OwnedIndexUpdates::new(database.clone());
        (scratch, root, database, updates)
    }

    #[test]
    fn sync_path_stores_one_row_for_non_canonical_spellings() {
        let directory = tempdir().unwrap();
        let (_scratch, root, database, updates) = scanned_canonical_root(directory.path());
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();

        // A `..` spelling of the file refreshes its row instead of
        // storing a second one for the same bytes on disk.
        updates
            .synchronize_path(&root.join("sub").join("..").join("clip.mp4"))
            .unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        let paths = indexed_display_paths(&observer);
        assert_eq!(paths.len(), 1);
        assert!(paths.contains(clip.to_string_lossy().as_ref()));
    }

    #[test]
    fn sync_path_removes_deleted_files_by_non_canonical_spelling() {
        let directory = tempdir().unwrap();
        let (_scratch, root, database, updates) = scanned_canonical_root(directory.path());
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        updates.synchronize_path(&clip).unwrap();

        std::fs::remove_file(&clip).unwrap();
        updates
            .synchronize_path(&root.join("sub").join("..").join("clip.mp4"))
            .unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        assert!(indexed_display_paths(&observer).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn sync_path_matches_roots_through_symlinked_ancestors() {
        let directory = tempdir().unwrap();
        let (_scratch, root, database, updates) = scanned_canonical_root(directory.path());
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        // Written through the link, so the bytes live under the root.
        std::fs::write(link.join("added.mp4"), b"video").unwrap();

        updates.synchronize_path(&link.join("added.mp4")).unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        let paths = indexed_display_paths(&observer);
        assert_eq!(paths.len(), 1);
        assert!(
            paths.contains(root.join("added.mp4").to_string_lossy().as_ref()),
            "{paths:?}"
        );
    }

    #[test]
    fn sync_rename_moves_a_file_through_non_canonical_spellings() {
        let directory = tempdir().unwrap();
        let (_scratch, root, database, updates) = scanned_canonical_root(directory.path());
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        updates.synchronize_path(&clip).unwrap();

        let renamed = root.join("clip-tagged.mp4");
        std::fs::rename(&clip, &renamed).unwrap();
        updates
            .synchronize_rename(
                &root.join("sub").join("..").join("clip.mp4"),
                &root.join("sub").join("..").join("clip-tagged.mp4"),
            )
            .unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        let paths = indexed_display_paths(&observer);
        assert_eq!(paths.len(), 1);
        assert!(paths.contains(renamed.to_string_lossy().as_ref()));
    }

    #[test]
    fn sync_rename_stores_the_canonical_spelling_of_a_moved_root() {
        let directory = tempdir().unwrap();
        let (_scratch, root, database, updates) = scanned_canonical_root(directory.path());
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        updates.synchronize_path(&clip).unwrap();

        let moved = directory.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        let moved_canonical = moved.canonicalize().unwrap();
        updates
            .synchronize_rename(
                &root,
                &directory.path().join("media").join("..").join("moved"),
            )
            .unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        assert_eq!(
            observer.roots().unwrap(),
            vec![("root".to_string(), moved_canonical.clone())]
        );
        assert!(indexed_display_paths(&observer)
            .contains(moved_canonical.join("clip.mp4").to_string_lossy().as_ref()));
    }

    #[test]
    fn renaming_a_nested_root_moves_the_exact_root_row() {
        let directory = tempdir().unwrap();
        let parent = directory.path().join("watched");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("clip.mp4"), b"video").unwrap();
        let parent = parent.canonicalize().unwrap();
        let child = parent.join("child");
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        // The container sorts first by id: without exact-before-containing
        // selection the rename would resolve to it and strand the child row.
        index.insert_root("a-parent", &parent).unwrap();
        index.insert_root("z-child", &child).unwrap();
        index.scan_root("a-parent", &parent, |_| {}).unwrap();
        index.scan_root("z-child", &child, |_| {}).unwrap();
        drop(index);

        let renamed = parent.join("renamed");
        std::fs::rename(&child, &renamed).unwrap();
        let updates = OwnedIndexUpdates::new(database.clone());
        assert_eq!(
            updates.synchronize_rename(&child, &renamed).unwrap(),
            SyncOutcome::Matched
        );

        let observer = OwnedIndex::open(&database).unwrap();
        let roots = observer.roots().unwrap();
        assert!(roots.contains(&("a-parent".to_string(), parent.clone())));
        assert!(
            roots.contains(&("z-child".to_string(), renamed.clone())),
            "the child roots row must follow its directory: {roots:?}"
        );
        let paths = indexed_display_paths(&observer);
        assert_eq!(paths.len(), 1);
        assert!(paths.contains(renamed.join("clip.mp4").to_string_lossy().as_ref()));
    }

    #[test]
    fn sync_path_matches_recorded_roots_that_predate_canonical_storage() {
        // A roots row recorded under a non-canonical spelling (a root
        // re-pointed through a symlinked parent, say) still matches: the
        // comparison normalizes both sides.
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir_all(&root).unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        drop(index);
        let updates = OwnedIndexUpdates::new(database.clone());

        let added = root.canonicalize().unwrap().join("added.mp4");
        std::fs::write(&added, b"video").unwrap();
        updates.synchronize_path(&added).unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        assert!(
            indexed_display_paths(&observer).contains(added.to_string_lossy().as_ref()),
            "a canonical sync must still match a non-canonical recorded root"
        );
    }

    #[cfg(unix)]
    #[test]
    fn sync_migrates_a_legacy_root_to_its_canonical_spelling() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let clip = real.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // The legacy row: recorded through the symlink, with a file
        // already scanned under the recorded spelling.
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &link).unwrap();
        index.scan_root("root", &link, |_| {}).unwrap();
        drop(index);
        let canonical_real = real.canonicalize().unwrap();
        let canonical_clip = canonical_real.join("clip.mp4");

        // Refreshing the scanned file rewrites the legacy rows instead of
        // storing a canonical twin beside them.
        let updates = OwnedIndexUpdates::new(database.clone());
        updates.synchronize_path(&canonical_clip).unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        let paths = indexed_display_paths(&observer);
        assert_eq!(paths.len(), 1);
        assert!(paths.contains(canonical_clip.to_string_lossy().as_ref()));
        assert_eq!(
            observer.roots().unwrap(),
            vec![("root".to_string(), canonical_real.clone())]
        );
        let folders_search: String = observer
            .connection
            .query_row("SELECT folders_search FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            folders_search,
            canonical_real.to_string_lossy().to_lowercase()
        );

        // The rename side heals the same way: no twin rows, no orphans.
        let tagged = real.join("clip-tagged.mp4");
        std::fs::rename(&clip, &tagged).unwrap();
        let canonical_tagged = canonical_real.join("clip-tagged.mp4");
        updates
            .synchronize_rename(&canonical_clip, &canonical_tagged)
            .unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        let paths = indexed_display_paths(&observer);
        assert_eq!(paths.len(), 1);
        assert!(paths.contains(canonical_tagged.to_string_lossy().as_ref()));
    }

    #[cfg(unix)]
    #[test]
    fn sync_skips_migration_when_another_root_holds_the_canonical_spelling() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical_real = real.canonicalize().unwrap();
        // Twins for one directory: a legacy alias row beside its canonical twin.
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("legacy", &link).unwrap();
        index.insert_root("canonical", &canonical_real).unwrap();
        index
            .scan_root("canonical", &canonical_real, |_| {})
            .unwrap();
        drop(index);

        // Neither sync may fail UNIQUE; both roots stay usable.
        let added = canonical_real.join("added.mp4");
        std::fs::write(&added, b"video").unwrap();
        let updates = OwnedIndexUpdates::new(database.clone());
        assert_eq!(
            updates.synchronize_path(&added).unwrap(),
            SyncOutcome::Matched
        );
        // The path sync links the file to both twins.
        let links_after_path: i64 = OwnedIndex::open(&database)
            .unwrap()
            .connection
            .query_row(
                "SELECT COUNT(*) FROM root_files
                 JOIN files ON files.id = root_files.file_id
                 WHERE files.path = ?1",
                [path_bytes(&added)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(links_after_path, 2);

        let renamed = canonical_real.join("renamed.mp4");
        std::fs::rename(&added, &renamed).unwrap();
        assert_eq!(
            updates.synchronize_rename(&added, &renamed).unwrap(),
            SyncOutcome::Matched
        );

        let observer = OwnedIndex::open(&database).unwrap();
        // The legacy row keeps its spelling (no merge), still matching
        // transiently; the rename moves the one file row. Its membership
        // reconciliation uses recorded spellings, so the alias link drops
        // until the next path sync re-links it — the pre-migration
        // transient behavior for twins, not a second error.
        let roots = observer.roots().unwrap();
        assert!(roots.contains(&("legacy".to_string(), link.clone())));
        assert!(roots.contains(&("canonical".to_string(), canonical_real.clone())));
        let paths = indexed_display_paths(&observer);
        assert_eq!(paths.len(), 1);
        assert!(paths.contains(renamed.to_string_lossy().as_ref()));
        drop(observer);

        // Both twins stay scannable: the alias scan keeps its recorded
        // spelling instead of failing UNIQUE against the canonical twin.
        let mut scanner = OwnedIndex::open(&database).unwrap();
        scanner.scan_root("legacy", &link, |_| {}).unwrap();
        scanner
            .scan_root("canonical", &canonical_real, |_| {})
            .unwrap();
        let roots = scanner.roots().unwrap();
        assert!(roots.contains(&("legacy".to_string(), link)));
        assert!(roots.contains(&("canonical".to_string(), canonical_real)));
    }

    #[cfg(unix)]
    #[test]
    fn scan_with_a_non_canonical_root_spelling_walks_and_stores_canonical() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("clip.mp4"), b"video").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical_real = real.canonicalize().unwrap();
        let canonical_clip = canonical_real.join("clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &link).unwrap();

        // A stale spelling in still walks the directory it names and stores
        // the canonical rows, migrating the recorded row with them.
        index.scan_root("root", &link, |_| {}).unwrap();
        assert_eq!(
            index.roots().unwrap(),
            vec![("root".to_string(), canonical_real.clone())]
        );
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([canonical_clip.to_string_lossy().into_owned()])
        );

        // And a later stale scan never reverts the migrated row.
        index.scan_root("root", &link, |_| {}).unwrap();
        assert_eq!(
            index.roots().unwrap(),
            vec![("root".to_string(), canonical_real)]
        );
        assert_eq!(indexed_file_count(&index), 1);
    }

    #[cfg(unix)]
    #[test]
    fn migration_merges_a_file_twin_already_stored_under_the_canonical_spelling() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::write(real.join("sub").join("clip.mp4"), b"video").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical_real = real.canonicalize().unwrap();
        let canonical_sub = canonical_real.join("sub");
        let canonical_clip = canonical_sub.join("clip.mp4");
        let alias_clip = link.join("sub").join("clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("alias", &link).unwrap();
        index.insert_root("sub", &canonical_sub).unwrap();
        index.scan_root("sub", &canonical_sub, |_| {}).unwrap();
        // The alias row's own spelling of the same bytes, as a pre-migration
        // scan would have stored it: a twin the migration must merge.
        index
            .connection
            .execute(
                "INSERT INTO files (
                    path, display_path, file_name, extension, media_type,
                    size_bytes, created_at_ns, modified_at_ns,
                    file_name_search, tags_search, folders_search
                 ) SELECT ?1, ?2, file_name, extension, media_type, size_bytes,
                    created_at_ns, modified_at_ns,
                    file_name_search, tags_search, ?3
                 FROM files WHERE path = ?4",
                params![
                    path_bytes(&alias_clip),
                    alias_clip.to_string_lossy(),
                    folders_search_text(&alias_clip),
                    path_bytes(&canonical_clip),
                ],
            )
            .unwrap();
        let alias_id: i64 = index
            .connection
            .query_row(
                "SELECT id FROM files WHERE path = ?1",
                [path_bytes(&alias_clip)],
                |row| row.get(0),
            )
            .unwrap();
        let generation: i64 = index
            .connection
            .query_row(
                "SELECT scan_generation FROM roots WHERE id = 'alias'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        index
            .connection
            .execute(
                "INSERT INTO root_files (root_id, file_id, scan_generation)
                 VALUES ('alias', ?1, ?2)",
                params![alias_id, generation],
            )
            .unwrap();
        drop(index);

        let updates = OwnedIndexUpdates::new(database.clone());
        updates.synchronize_path(&canonical_clip).unwrap();

        let observer = OwnedIndex::open(&database).unwrap();
        assert_eq!(
            indexed_display_paths(&observer),
            BTreeSet::from([canonical_clip.to_string_lossy().into_owned()])
        );
        let roots = observer.roots().unwrap();
        assert!(roots.contains(&("alias".to_string(), canonical_real.clone())));
        assert!(roots.contains(&("sub".to_string(), canonical_sub.clone())));
        let links: i64 = observer
            .connection
            .query_row(
                "SELECT COUNT(*) FROM root_files
                 JOIN files ON files.id = root_files.file_id
                 WHERE files.path = ?1",
                [path_bytes(&canonical_clip)],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(links, 2);
    }

    #[cfg(unix)]
    #[test]
    fn migration_leaves_files_shared_with_another_root_alone() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        let clip = real.join("sub").join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical_real = real.canonicalize().unwrap();
        let alias_clip = link.join("sub").join("clip.mp4");
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("a", &link).unwrap();
        index.insert_root("p", &link.join("sub")).unwrap();
        // One row for the same bytes, linked to both legacy rows.
        let metadata = std::fs::metadata(&alias_clip).unwrap();
        let file = IndexedFile::read(alias_clip.clone(), metadata).unwrap();
        index
            .connection
            .execute(
                "INSERT INTO files (
                    path, display_path, file_name, extension, media_type,
                    size_bytes, created_at_ns, modified_at_ns,
                    file_name_search, tags_search, folders_search
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
            )
            .unwrap();
        let file_id: i64 = index
            .connection
            .query_row("SELECT id FROM files", [], |row| row.get(0))
            .unwrap();
        for root_id in ["a", "p"] {
            let generation: i64 = index
                .connection
                .query_row(
                    "SELECT scan_generation FROM roots WHERE id = ?1",
                    [root_id],
                    |row| row.get(0),
                )
                .unwrap();
            index
                .connection
                .execute(
                    "INSERT INTO root_files (root_id, file_id, scan_generation)
                     VALUES (?1, ?2, ?3)",
                    params![root_id, file_id, generation],
                )
                .unwrap();
        }
        drop(index);

        // A sync for a missing path still migrates the rows first, but the
        // shared file keeps its spelling for the other root.
        let updates = OwnedIndexUpdates::new(database.clone());
        let gone = canonical_real.join("sub").join("gone.mp4");
        assert_eq!(
            updates.synchronize_path(&gone).unwrap(),
            SyncOutcome::Matched
        );

        let observer = OwnedIndex::open(&database).unwrap();
        assert_eq!(
            indexed_display_paths(&observer),
            BTreeSet::from([alias_clip.to_string_lossy().into_owned()])
        );
        let roots = observer.roots().unwrap();
        assert!(roots.contains(&("a".to_string(), canonical_real.clone())));
        assert!(roots.contains(&("p".to_string(), canonical_real.join("sub"))));
        let links: i64 = observer
            .connection
            .query_row(
                "SELECT COUNT(*) FROM root_files WHERE file_id = ?1",
                [file_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(links, 2);
    }

    #[cfg(unix)]
    #[test]
    fn migration_skips_a_root_repointed_after_it_was_read() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let clip = real.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let canonical_real = real.canonicalize().unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &link).unwrap();
        index_legacy_file_row(&index, "root", &link.join("clip.mp4"));

        // A concurrent rename lands between the caller's roots() read and
        // the migration's commit: the row no longer holds `link`.
        let moved = directory.path().join("moved");
        index
            .connection
            .execute(
                "UPDATE roots SET path = ?1, display_path = ?2 WHERE id = 'root'",
                params![path_bytes(&moved), moved.to_string_lossy()],
            )
            .unwrap();
        let before = dump_index_state(&index);

        // The migration still believes the recorded spelling is `link`;
        // the conditional update must match no row and change nothing.
        index
            .migrate_root_spelling("root", &link, &canonical_real)
            .unwrap();
        assert_eq!(dump_index_state(&index), before);
        assert_eq!(index.roots().unwrap(), vec![("root".to_string(), moved)]);
    }

    #[test]
    fn sync_outcomes_report_whether_a_watched_folder_matched() {
        let directory = tempdir().unwrap();
        let (_scratch, root, _database, updates) = scanned_canonical_root(directory.path());
        let clip = root.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        let outside = directory.path().join("elsewhere.mp4");
        std::fs::write(&outside, b"video").unwrap();

        assert_eq!(
            updates.synchronize_path(&clip).unwrap(),
            SyncOutcome::Matched
        );
        assert_eq!(
            updates.synchronize_path(&outside).unwrap(),
            SyncOutcome::Unmatched
        );
        let renamed = root.join("clip-tagged.mp4");
        std::fs::rename(&clip, &renamed).unwrap();
        assert_eq!(
            updates.synchronize_rename(&clip, &renamed).unwrap(),
            SyncOutcome::Matched
        );
        assert_eq!(
            updates
                .synchronize_rename(&outside, &root.join("moved-in.mp4"))
                .unwrap(),
            SyncOutcome::Unmatched
        );

        let missing = OwnedIndexUpdates::new(directory.path().join("no-index.sqlite3"));
        assert_eq!(
            missing.synchronize_path(&clip).unwrap(),
            SyncOutcome::NoIndex
        );
        assert_eq!(
            missing.synchronize_rename(&clip, &renamed).unwrap(),
            SyncOutcome::NoIndex
        );
    }

    #[cfg(unix)]
    #[test]
    fn unmatched_syncs_leave_a_legacy_index_untouched() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let clip = real.join("clip.mp4");
        std::fs::write(&clip, b"video").unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &link).unwrap();
        index_legacy_file_row(&index, "root", &link.join("clip.mp4"));
        let before = dump_index_state(&index);
        drop(index);

        // Neither unmatched sync may migrate the legacy row it never
        // matched, nor bump the revision doing it.
        let updates = OwnedIndexUpdates::new(database.clone());
        let elsewhere = directory.path().join("elsewhere.mp4");
        std::fs::write(&elsewhere, b"video").unwrap();
        assert_eq!(
            updates.synchronize_path(&elsewhere).unwrap(),
            SyncOutcome::Unmatched
        );
        assert_eq!(
            updates
                .synchronize_rename(&elsewhere, &directory.path().join("moved.mp4"))
                .unwrap(),
            SyncOutcome::Unmatched
        );

        let observer = OwnedIndex::open(&database).unwrap();
        assert_eq!(dump_index_state(&observer), before);
        assert_eq!(observer.roots().unwrap(), vec![("root".to_string(), link)]);
    }

    #[test]
    fn sync_surfaces_an_unreadable_index_instead_of_skipping_it() {
        let directory = tempdir().unwrap();
        // A name the filesystem refuses to even look up stands in for any
        // unreadable database path (an unsearchable parent, a denied file):
        // only a truly missing index may read as "no index".
        let database = directory.path().join("x".repeat(300)).join("index.sqlite3");
        let updates = OwnedIndexUpdates::new(database);

        assert!(updates
            .synchronize_path(&directory.path().join("clip.mp4"))
            .is_err());
        assert!(updates
            .synchronize_rename(
                &directory.path().join("a.mp4"),
                &directory.path().join("b.mp4")
            )
            .is_err());
    }

    #[test]
    fn sync_against_a_directory_at_the_database_path_is_an_error() {
        let directory = tempdir().unwrap();
        // A directory where the database belongs is not a missing index:
        // both syncs fail honestly instead of skipping quietly.
        let database = directory.path().join("index.sqlite3");
        std::fs::create_dir(&database).unwrap();
        let updates = OwnedIndexUpdates::new(database);

        assert!(updates
            .synchronize_path(&directory.path().join("clip.mp4"))
            .is_err());
        assert!(updates
            .synchronize_rename(
                &directory.path().join("a.mp4"),
                &directory.path().join("b.mp4")
            )
            .is_err());
    }

    #[test]
    fn a_rescan_flag_repairs_events_the_watcher_may_have_missed() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let missed = root.join("missed.mp4");
        std::fs::write(&missed, b"video").unwrap();
        let event = Event::new(EventKind::Any).set_flag(notify::event::Flag::Rescan);
        let mut state = NotifyEventState::default();

        let shared = WorkerShared::new();
        apply_notify_event(
            &mut index,
            "root",
            &root,
            &shared,
            &mut state,
            event,
            &mut |_| {},
        )
        .unwrap();

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
        OwnedIndex::open(&database)
            .unwrap()
            .insert_root("root", &root)
            .unwrap();

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
        #[cfg(unix)]
        crate::indexer_daemon::test_runtime_dir();
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let database = directory.path().join("index.sqlite3");
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        OwnedIndex::open(&database)
            .unwrap()
            .insert_root("root", &root)
            .unwrap();
        let restored_manager = OwnedIndexManager::system_under(database).unwrap();

        // A completion wait, not a race: a one-file scan finishes in
        // milliseconds unloaded, so this bound only guards a genuinely
        // stuck restore — and reports the last state if it trips.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        loop {
            let state = restored_manager.state().unwrap();
            if state.folders[0].status == OwnedFolderStatus::Ready {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "restore never completed; last saw: {state:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(restored_manager.state().unwrap().indexed_videos, 1);
        restored_manager.remove_folder("root").unwrap();
    }

    // The supervision tests never drop a guard and re-acquire it: other
    // test threads fork short-lived children (every `systemctl` probe) that
    // inherit open lock files across the fork, so a just-released lock can
    // read as held for microseconds. Each test below holds its side
    // continuously instead, which asserts the same mutual exclusion
    // deterministically.
    #[test]
    #[cfg(unix)]
    fn a_system_manager_yields_supervision_to_a_running_daemon() {
        crate::indexer_daemon::test_runtime_dir();
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let database = directory.path().join("index.sqlite3");
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        OwnedIndex::open(&database)
            .unwrap()
            .insert_root("root", &root)
            .unwrap();

        // A held lock reads as a running daemon: the manager starts no
        // watchers, but reads keep working.
        let _daemon = crate::indexer_daemon::try_acquire_daemon_lock(&database)
            .unwrap()
            .expect("test holds the daemon lock");
        let manager = OwnedIndexManager::system_under(database.clone()).unwrap();
        assert!(!manager.supervises_watchers().unwrap());
        assert!(manager.watchers.lock().unwrap().is_empty());
        assert_eq!(manager.state().unwrap().folders.len(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn a_system_manager_creates_a_missing_index_directory_before_locking() {
        crate::indexer_daemon::test_runtime_dir();
        let directory = tempdir().unwrap();
        let parent = directory.path().join("missing-index-folder");
        let database = parent.join("index.sqlite3");

        let manager = OwnedIndexManager::system_under(database.clone()).unwrap();

        assert!(parent.is_dir(), "the index parent must be created first");
        assert!(manager.supervises_watchers().unwrap());
        assert!(
            crate::indexer_daemon::try_acquire_daemon_lock(&database)
                .unwrap()
                .is_none(),
            "the manager must hold the daemon lock"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_yielded_manager_resumes_supervision_once_the_daemon_stops() {
        crate::indexer_daemon::test_runtime_dir();
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let database = directory.path().join("index.sqlite3");
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        OwnedIndex::open(&database)
            .unwrap()
            .insert_root("root", &root)
            .unwrap();

        // The daemon holds the lock: the manager starts yielded, with no
        // watchers of its own.
        let daemon = crate::indexer_daemon::try_acquire_daemon_lock(&database)
            .unwrap()
            .expect("test holds the daemon lock");
        let manager = OwnedIndexManager::system_under(database.clone()).unwrap();
        assert!(!manager.supervises_watchers().unwrap());
        assert!(manager.watchers.lock().unwrap().is_empty());
        drop(daemon);

        // A just-released lock can read as held for microseconds (forked
        // children inherit the lock file), so the re-acquisition retries
        // until a reconcile takes over supervision and restores watchers.
        wait_for(|| {
            manager.ensure_watchers().unwrap();
            manager.supervises_watchers().unwrap()
        });
        assert_eq!(manager.watchers.lock().unwrap().len(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn a_yielded_manager_recreates_a_reset_index_directory_while_reacquiring() {
        crate::indexer_daemon::test_runtime_dir();
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        // The index lives in its own folder so the reset below wipes only
        // it, never the watched media.
        let parent = directory.path().join("index-folder");
        let database = parent.join("index.sqlite3");
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        OwnedIndex::open(&database)
            .unwrap()
            .insert_root("root", &root)
            .unwrap();

        // The daemon holds the lock: the manager starts yielded, with no
        // watchers of its own.
        let daemon = crate::indexer_daemon::try_acquire_daemon_lock(&database)
            .unwrap()
            .expect("test holds the daemon lock");
        let manager = OwnedIndexManager::system_under(database.clone()).unwrap();
        assert!(!manager.supervises_watchers().unwrap());

        // A reset while yielded wipes the index folder — including the
        // legacy lock file, though never the primary one; the daemon stops
        // with it.
        std::fs::remove_dir_all(&parent).unwrap();
        drop(daemon);

        // The next poll recreates the parent and re-acquires instead of
        // erroring ENOENT on every poll until restart. A just-released
        // lock can read as held for microseconds (forked children
        // inherit the lock file), so the re-acquisition retries until a
        // reconcile takes over supervision.
        wait_for(|| {
            manager.ensure_watchers().unwrap();
            manager.supervises_watchers().unwrap()
        });
        assert!(parent.is_dir());
        assert!(
            crate::indexer_daemon::try_acquire_daemon_lock(&database)
                .unwrap()
                .is_none(),
            "the manager must hold the daemon lock"
        );
        // Supervision actually resumes: a folder added now gets a watcher.
        manager.add_folder(&root).unwrap();
        assert_eq!(manager.watchers.lock().unwrap().len(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn a_supervising_system_manager_refuses_a_second_supervisor() {
        crate::indexer_daemon::test_runtime_dir();
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");

        let manager = OwnedIndexManager::system_under(database.clone()).unwrap();

        assert!(manager.supervises_watchers().unwrap());
        assert!(
            crate::indexer_daemon::try_acquire_daemon_lock(&database)
                .unwrap()
                .is_none(),
            "the daemon side must yield to the app's watchers"
        );
    }

    #[test]
    #[cfg(unix)]
    fn two_system_managers_yield_to_whichever_started_first() {
        crate::indexer_daemon::test_runtime_dir();
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");

        // Two coeval managers, not a manager against a bare lock: the
        // second yields supervision while its reads keep working.
        let first = OwnedIndexManager::system_under(database.clone()).unwrap();
        assert!(first.supervises_watchers().unwrap());
        let second = OwnedIndexManager::system_under(database.clone()).unwrap();

        assert!(!second.supervises_watchers().unwrap());
        assert!(second.watchers.lock().unwrap().is_empty());
        assert!(second.state().unwrap().folders.is_empty());
        assert!(first.supervises_watchers().unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn system_under_reports_an_index_directory_it_cannot_secure() {
        // Root can chmod anything, so this refusal only exists for the
        // rest of us.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        // The filesystem root always exists (so ensuring it gets past
        // creation) and is never securable by an unprivileged user.
        let database = PathBuf::from("/").join("toka-test-index.sqlite3");

        let error = OwnedIndexManager::system_under(database)
            .err()
            .expect("system_under must refuse an unsecurable index directory");

        assert!(
            error.contains("could not secure"),
            "unexpected error: {error}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn adding_a_folder_without_supervision_starts_no_watcher() {
        crate::indexer_daemon::test_runtime_dir();
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let database = directory.path().join("state").join("index.sqlite3");
        let _daemon = crate::indexer_daemon::try_acquire_daemon_lock(&database)
            .unwrap()
            .expect("test holds the daemon lock");
        let manager = OwnedIndexManager::system_under(database).unwrap();

        let state = manager.add_folder(&root).unwrap();

        // The folder is recorded for the daemon to pick up; only the
        // in-process watcher is skipped.
        assert_eq!(state.folders.len(), 1);
        assert!(manager.watchers.lock().unwrap().is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn the_held_lock_check_sees_the_app_supervisor() {
        crate::indexer_daemon::test_runtime_dir();
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");

        // The app holds the same primary lock as a daemon (plus the
        // legacy co-take), so purge and reset refuse under it too.
        let manager = OwnedIndexManager::system_under(database.clone()).unwrap();
        assert!(manager.supervises_watchers().unwrap());
        assert!(database.with_file_name("daemon.lock").is_file());
        assert!(
            crate::indexer_daemon::daemon_lock_held(&database).unwrap(),
            "an app-held lock must block a purge or reset"
        );
        drop(manager);

        // A just-released lock can read as held for microseconds (forked
        // children inherit the lock file), so the release polls.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while crate::indexer_daemon::daemon_lock_held(&database).unwrap() {
            assert!(
                std::time::Instant::now() < deadline,
                "the released lock never cleared"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
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
        OwnedIndex::open(&database)
            .unwrap()
            .insert_root("root", &root)
            .unwrap();
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
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let added = root.join("added");
        std::fs::create_dir_all(added.join("nested")).unwrap();
        std::fs::write(added.join("one.mp4"), b"one").unwrap();
        std::fs::write(added.join("nested/two.jpg"), b"two").unwrap();

        index
            .synchronize_path("root", &root, &added, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([
                added.join("one.mp4").to_string_lossy().into_owned(),
                added.join("nested/two.jpg").to_string_lossy().into_owned(),
            ])
        );

        std::fs::remove_dir_all(&added).unwrap();
        index
            .synchronize_path("root", &root, &added, &AtomicBool::new(false))
            .unwrap();
        assert!(indexed_display_paths(&index).is_empty());
    }

    #[test]
    fn watcher_buffers_events_while_the_initial_scan_runs() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let root = root.canonicalize().unwrap();
        let nested = root.join("nested");
        std::fs::write(nested.join("existing.mp4"), b"video").unwrap();
        let database = directory.path().join("index.sqlite3");
        OwnedIndex::open(&database)
            .unwrap()
            .insert_root("root", &root)
            .unwrap();
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
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();

        assert!(index
            .synchronize_path("missing-root", &root, &clip, &AtomicBool::new(false))
            .is_err());
        drop(index);

        let mut reopened = OwnedIndex::open(&database).unwrap();
        let repairs = reopened.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, clip);
        assert_eq!(repairs[0].reason, "direct update");
        assert!(repairs[0].last_error.is_some());

        let cancel = AtomicBool::new(false);
        assert_eq!(
            reopened
                .repair_pending("root", &root, &cancel, |_| {})
                .unwrap(),
            1
        );
        assert!(reopened.pending_repairs().unwrap().is_empty());
    }

    #[test]
    fn paired_rename_events_move_a_directory_as_one_index_change() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let from = root.join("before");
        std::fs::create_dir_all(&from).unwrap();
        let root = root.canonicalize().unwrap();
        let from = root.join("before");
        std::fs::write(from.join("clip.mp4"), b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let to = root.join("after");
        std::fs::rename(&from, &to).unwrap();
        let mut state = NotifyEventState::default();

        let from_event = Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::From,
        )))
        .add_path(from.clone())
        .set_tracker(7);
        let shared = WorkerShared::new();
        apply_notify_event(
            &mut index,
            "root",
            &root,
            &shared,
            &mut state,
            from_event,
            &mut |_| {},
        )
        .unwrap();
        assert!(indexed_display_paths(&index)
            .contains(from.join("clip.mp4").to_string_lossy().as_ref()));

        let to_event = Event::new(EventKind::Modify(notify::event::ModifyKind::Name(
            notify::event::RenameMode::To,
        )))
        .add_path(to.clone())
        .set_tracker(7);
        apply_notify_event(
            &mut index,
            "root",
            &root,
            &shared,
            &mut state,
            to_event,
            &mut |_| {},
        )
        .unwrap();
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
        let root = root.canonicalize().unwrap();
        let from = root.join("clip.mp4");
        let to = root.join("clip [home].mp4");
        std::fs::write(&from, b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        std::fs::rename(&from, &to).unwrap();

        assert!(index
            .synchronize_rename("missing-root", &root, &from, &to, &AtomicBool::new(false))
            .is_err());

        let repairs = index.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, to);
        assert_eq!(repairs[0].reason, "direct rename");

        let cancel = AtomicBool::new(false);
        assert_eq!(
            index
                .repair_pending("root", &root, &cancel, |_| {})
                .unwrap(),
            1
        );
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
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let renamed = root.parent().unwrap().join("media [home]");
        std::fs::rename(&root, &renamed).unwrap();

        index
            .synchronize_rename("root", &root, &root, &renamed, &AtomicBool::new(false))
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
        let root = root.canonicalize().unwrap();
        let outside = outside.canonicalize().unwrap();
        let inside = root.join("clip.mp4");
        let moved_out = outside.join("clip.mp4");
        std::fs::write(&inside, b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();

        std::fs::rename(&inside, &moved_out).unwrap();
        index
            .synchronize_rename("root", &root, &inside, &moved_out, &AtomicBool::new(false))
            .unwrap();
        assert!(indexed_display_paths(&index).is_empty());

        std::fs::rename(&moved_out, &inside).unwrap();
        index
            .synchronize_rename("root", &root, &moved_out, &inside, &AtomicBool::new(false))
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
        let parent = parent.canonicalize().unwrap();
        let child = parent.join("photos");
        let from = child.join("trip.jpg");
        let to = parent.join("trip.jpg");
        std::fs::write(&from, b"photo").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("parent", &parent).unwrap();
        index.scan_root("parent", &parent, |_| {}).unwrap();
        index.insert_root("child", &child).unwrap();
        index.scan_root("child", &child, |_| {}).unwrap();
        std::fs::rename(&from, &to).unwrap();

        index
            .synchronize_rename("parent", &parent, &from, &to, &AtomicBool::new(false))
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
        let root = root.canonicalize().unwrap();
        let old_folder = root.join("beforeqzxv-folder");
        std::fs::write(old_folder.join("one.mp4"), b"one").unwrap();
        std::fs::write(old_folder.join("nested/two.jpg"), b"two").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let new_folder = root.join("afterqzxv-folder");

        std::fs::rename(&old_folder, &new_folder).unwrap();
        index
            .synchronize_rename(
                "root",
                &root,
                &old_folder,
                &new_folder,
                &AtomicBool::new(false),
            )
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
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        let clip = root.join("clip.mp4");

        std::fs::write(&clip, b"one").unwrap();
        index
            .synchronize_path("root", &root, &clip, &AtomicBool::new(false))
            .unwrap();
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([clip.to_string_lossy().into_owned()])
        );

        std::fs::write(&clip, b"a larger video").unwrap();
        index
            .synchronize_path("root", &root, &clip, &AtomicBool::new(false))
            .unwrap();
        let size: i64 = index
            .connection
            .query_row("SELECT size_bytes FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(size, 14);

        std::fs::remove_file(&clip).unwrap();
        index
            .synchronize_path("root", &root, &clip, &AtomicBool::new(false))
            .unwrap();
        assert!(indexed_display_paths(&index).is_empty());
    }

    #[test]
    fn a_managed_root_reports_scanned_media_and_folder_state() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        std::fs::write(root.join("photo.jpg"), b"image").unwrap();
        let manager =
            OwnedIndexManager::under(directory.path().join("state").join("index.sqlite3"));

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
    fn scanning_a_removed_root_reports_it_is_not_configured() {
        let (_directory, mut index) = open_index();
        let error = index
            .scan_root("gone", Path::new("/gone"), |_| {})
            .unwrap_err();
        assert!(matches!(error, OwnedIndexError::FolderNotConfigured));
    }

    #[test]
    fn folders_report_no_current_path_before_and_after_a_scan() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        let manager =
            OwnedIndexManager::under(directory.path().join("state").join("index.sqlite3"));

        let added = manager.add_folder(&root).unwrap();
        assert_eq!(added.folders[0].current_path, None);
        assert!(added.folders[0].recent_directories.is_empty());

        let id = added.folders[0].id.clone();
        OwnedIndex::open(&manager.database)
            .unwrap()
            .scan_root(&id, &root, |_| {})
            .unwrap();

        let state = manager.state().unwrap();
        assert_eq!(state.folders[0].status, OwnedFolderStatus::Ready);
        assert_eq!(state.folders[0].current_path, None);
        assert!(state.folders[0].recent_directories.is_empty());
    }

    #[test]
    fn indexing_folders_report_the_directory_they_are_scanning() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        let database = directory.path().join("state").join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());

        let added = manager.add_folder(&root).unwrap();
        let id = added.folders[0].id.clone();

        let observed = std::cell::RefCell::new(Vec::new());
        OwnedIndex::open(&database)
            .unwrap()
            .scan_root_with_batch_size(&id, &root, 1, |progress| {
                if progress.phase == ScanPhase::Scanning && progress.current_folder.is_some() {
                    let folder = manager.state().unwrap().folders.remove(0);
                    observed
                        .borrow_mut()
                        .push((folder.status, folder.current_path));
                }
            })
            .unwrap();

        let observed = observed.borrow();
        assert!(
            !observed.is_empty(),
            "expected to observe the scan while it was running"
        );
        assert!(observed
            .iter()
            .all(|(status, _)| *status == OwnedFolderStatus::Indexing));
        assert!(
            observed
                .iter()
                .any(|(_, path)| *path == Some(root.to_string_lossy().into_owned())),
            "expected the scanned directory to be reported, saw {observed:?}"
        );
    }

    #[test]
    fn indexing_folders_track_directories_without_media_batches() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let trips = root.join("Trips");
        std::fs::create_dir_all(&trips).unwrap();
        let root = root.canonicalize().unwrap();
        let trips = root.join("Trips");
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        // No supported media under Trips, so no batch ever commits there.
        std::fs::write(trips.join("notes.txt"), b"notes").unwrap();
        let database = directory.path().join("state").join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());

        let added = manager.add_folder(&root).unwrap();
        let id = added.folders[0].id.clone();

        let observed = std::cell::RefCell::new(Vec::new());
        OwnedIndex::open(&database)
            .unwrap()
            .scan_root(&id, &root, |progress| {
                if progress.current_folder.as_deref() == Some(trips.as_path()) {
                    let folder = manager.state().unwrap().folders.remove(0);
                    observed
                        .borrow_mut()
                        .push((folder.status, folder.current_path));
                }
            })
            .unwrap();

        let observed = observed.borrow();
        assert!(
            observed
                .iter()
                .any(|(status, path)| *status == OwnedFolderStatus::Indexing
                    && *path == Some(trips.to_string_lossy().into_owned())),
            "expected Trips to be reported while scanning it, saw {observed:?}"
        );
    }

    // Linux-only: only filesystems like ext4/tmpfs accept non-UTF-8 names;
    // macOS (APFS) rejects them at creation, so this scenario cannot occur
    // there. The lossy conversion itself is platform-neutral.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_directories_do_not_break_folder_state() {
        use std::os::unix::ffi::OsStringExt;

        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let odd = root.join(std::ffi::OsString::from_vec(b"\xff\xfe".to_vec()));
        std::fs::create_dir_all(&odd).unwrap();
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());

        let added = manager.add_folder(&root).unwrap();
        let id = added.folders[0].id.clone();

        let mut serialized_while_inside = false;
        OwnedIndex::open(&database)
            .unwrap()
            .scan_root(&id, &root, |progress| {
                if progress.current_folder.as_deref() == Some(odd.as_path()) {
                    let state = manager.state().unwrap();
                    // Serialization is what the index_state command does; it
                    // must not fail on the raw directory bytes.
                    let folder = serde_json::to_value(&state.folders[0]).unwrap();
                    assert_eq!(
                        folder.get("currentPath"),
                        Some(&serde_json::Value::String(
                            odd.to_string_lossy().into_owned()
                        )),
                        "currentPath should be the lossy directory name, saw {folder}"
                    );
                    serialized_while_inside = true;
                }
            })
            .unwrap();

        assert!(
            serialized_while_inside,
            "expected to observe the scan inside the non-UTF-8 directory"
        );
    }

    // Linux-only: only filesystems like ext4/tmpfs accept non-UTF-8 names;
    // macOS (APFS) rejects them at creation, so this scenario cannot occur
    // there. The lossy conversion itself is platform-neutral.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_roots_serialize_lossy_in_folder_state_and_stats() {
        use std::os::unix::ffi::OsStringExt;

        let directory = tempdir().unwrap();
        let root = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"media-\xff".to_vec()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database);

        manager.add_folder(&root).unwrap();

        let state = manager.state().unwrap();
        let folder = serde_json::to_value(&state.folders[0]).unwrap();
        assert_eq!(
            folder.get("path"),
            Some(&serde_json::Value::String(
                root.to_string_lossy().into_owned()
            )),
            "a non-UTF-8 root must serialize lossy, saw {folder}"
        );

        let stats = manager.stats().unwrap();
        let stats = serde_json::to_value(&stats).unwrap();
        assert_eq!(
            stats["folders"][0]["path"],
            root.to_string_lossy().into_owned()
        );
    }

    #[test]
    fn folder_state_uses_the_documented_json_field_names() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let manager =
            OwnedIndexManager::under(directory.path().join("state").join("index.sqlite3"));
        manager.add_folder(&root).unwrap();

        let state = manager.state().unwrap();
        let folder = serde_json::to_value(&state.folders[0]).unwrap();

        assert_eq!(
            folder.get("currentPath"),
            Some(&serde_json::Value::Null),
            "currentPath should serialize as null while pending, saw {folder}"
        );
        assert_eq!(
            folder.get("recentDirectories"),
            Some(&serde_json::Value::Array(Vec::new())),
            "recentDirectories should serialize as an empty list, saw {folder}"
        );
    }

    #[test]
    fn adding_and_removing_a_search_folder_advances_the_index_revision() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let manager =
            OwnedIndexManager::under(directory.path().join("state").join("index.sqlite3"));
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
        let manager =
            OwnedIndexManager::under(directory.path().join("state").join("index.sqlite3"));
        manager.add_folder(&root).unwrap();
        std::fs::remove_dir(&root).unwrap();

        let state = manager.state().unwrap();
        assert_eq!(state.folders[0].status, OwnedFolderStatus::Offline);
    }

    #[test]
    fn folders_lists_managed_roots_with_their_state() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let manager =
            OwnedIndexManager::under(directory.path().join("state").join("index.sqlite3"));
        manager.add_folder(&root).unwrap();

        let folders = manager.folders().unwrap();

        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].path, root);
        assert_eq!(folders[0].status, OwnedFolderStatus::Pending);
    }

    #[test]
    fn folders_without_a_database_is_empty_and_creates_nothing() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());

        let folders = manager.folders().unwrap();

        assert!(folders.is_empty());
        assert!(!database.exists());
    }

    #[test]
    #[cfg(unix)]
    fn stats_and_folders_on_a_metadata_failure_error_instead_of_reporting_empty() {
        let directory = tempdir().unwrap();
        // A component past NAME_MAX fails metadata with ENAMETOOLONG on
        // Unix for every user — unlike chmod tricks, which root sails
        // through.
        let database = directory.path().join("a".repeat(300)).join("index.sqlite3");
        let manager = OwnedIndexManager::under_read_only(database);

        let folders = manager.folders().unwrap_err();
        let stats = manager.stats().unwrap_err();

        assert!(
            folders.contains("could not read its search index"),
            "unexpected error: {folders}"
        );
        assert!(
            stats.contains("could not read its search index"),
            "unexpected error: {stats}"
        );
    }

    #[test]
    fn stats_and_folders_on_a_directory_at_the_index_path_error_instead_of_reporting_empty() {
        let directory = tempdir().unwrap();
        let manager = OwnedIndexManager::under_read_only(directory.path().to_path_buf());

        let folders = manager.folders().unwrap_err();
        let stats = manager.stats().unwrap_err();

        assert!(
            folders.contains("is not a file"),
            "unexpected error: {folders}"
        );
        assert!(stats.contains("is not a file"), "unexpected error: {stats}");
    }

    #[test]
    fn removing_a_managed_root_removes_its_files() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        let manager =
            OwnedIndexManager::under(directory.path().join("state").join("index.sqlite3"));
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
    fn folders_at_or_inside_the_index_directory_are_rejected() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("state").join("index.sqlite3");
        let index_dir = database.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&index_dir).unwrap();
        let inside = index_dir.join("media");
        std::fs::create_dir(&inside).unwrap();
        let manager = OwnedIndexManager::under(database);

        // The index directory itself.
        let error = manager.add_folder(&index_dir).unwrap_err();
        assert!(
            error.contains("its own index folder"),
            "unexpected error: {error}"
        );

        // A folder inside it (even spelled through `.`).
        let error = manager.add_folder(&inside).unwrap_err();
        assert!(
            error.contains("inside Toka's index folder"),
            "unexpected error: {error}"
        );
        let error = manager
            .add_folder(&index_dir.join(".").join("media"))
            .unwrap_err();
        assert!(
            error.contains("inside Toka's index folder"),
            "unexpected error: {error}"
        );

        // A folder next to it still adds.
        let outside = directory.path().join("media");
        std::fs::create_dir(&outside).unwrap();
        assert_eq!(manager.add_folder(&outside).unwrap().folders.len(), 1);
    }

    #[test]
    fn index_data_files_enumerate_the_known_files_only() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");

        let files = index_data_files(&database);
        let names: Vec<_> = files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            names,
            vec![
                "daemon.lock",
                "search-renames.json",
                "search-renames.lock",
                "index.sqlite3",
                "index.sqlite3-wal",
                "index.sqlite3-shm",
                "index.sqlite3-journal",
            ]
        );
        // Siblings of the database — never the directory itself.
        assert!(files.iter().all(|path| path.parent() == database.parent()));
    }

    #[test]
    fn overlapping_managed_roots_are_rejected() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let manager =
            OwnedIndexManager::under(directory.path().join("state").join("index.sqlite3"));
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
    fn adding_an_alias_of_a_legacy_recorded_folder_is_rejected() {
        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let database = directory.path().join("state").join("index.sqlite3");
        // A legacy row recorded through the symlink.
        OwnedIndex::open(&database)
            .unwrap()
            .insert_root("legacy", &link)
            .unwrap();
        let manager = OwnedIndexManager::under(database);

        let error = manager.add_folder(&real).unwrap_err();

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

    #[test]
    fn opening_a_version_1_database_migrates_it_without_touching_roots() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        {
            let mut index = OwnedIndex::open(&database).unwrap();
            index.insert_root("root", &root).unwrap();
            index.scan_root("root", &root, |_| {}).unwrap();
            // Rewind to version 1: drop the liveness table and the
            // indexable counter, plus both version markers.
            index
                .connection
                .execute_batch(
                    "DROP TABLE daemon_state;
                     ALTER TABLE scan_runs DROP COLUMN files_indexable;
                     UPDATE schema_meta SET value = 1 WHERE key = 'schema_version';",
                )
                .unwrap();
            index
                .connection
                .pragma_update(None, "user_version", 1)
                .unwrap();
        }

        let index = OwnedIndex::open(&database).unwrap();

        let version: i64 = index
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let meta: i64 = index
            .connection
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(meta, 2);
        // Roots, files, and progress survive; nothing needs rescanning.
        let roots = index.roots_with_state().unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].2, "ready");
        assert_eq!(indexed_display_paths(&index).len(), 1);
        assert!(index.read_daemon_state().unwrap().is_none());
        let columns: Vec<String> = index
            .connection
            .prepare("SELECT name FROM pragma_table_info('scan_runs')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            columns.contains(&"files_indexable".to_owned()),
            "{columns:?}"
        );
    }

    #[test]
    fn read_only_open_creates_nothing_when_the_index_is_missing() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("missing").join("index.sqlite3");

        let result = OwnedIndex::open_read_only(&database);

        assert!(result.is_err());
        assert!(!database.is_file());
        assert!(!database.parent().unwrap().exists());
    }

    #[test]
    fn read_only_open_leaves_a_version_1_database_untouched() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        {
            let mut index = OwnedIndex::open(&database).unwrap();
            index.insert_root("root", &root).unwrap();
            index.scan_root("root", &root, |_| {}).unwrap();
            // Rewind to version 1: no liveness table, no indexable
            // counter, version markers back to 1.
            index
                .connection
                .execute_batch(
                    "DROP TABLE daemon_state;
                     ALTER TABLE scan_runs DROP COLUMN files_indexable;
                     UPDATE schema_meta SET value = 1 WHERE key = 'schema_version';",
                )
                .unwrap();
            index
                .connection
                .pragma_update(None, "user_version", 1)
                .unwrap();
        }
        let before = std::fs::read(&database).unwrap();

        let index = OwnedIndex::open_read_only(&database).unwrap();

        // Still version 1, byte for byte: a read must never migrate.
        // (SQLite may still create ephemeral -shm/-wal sidecars beside
        // the database; those are its runtime state, not index state,
        // and every reader — including a version 1 app — creates them.)
        let version: i64 = index
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        drop(index);
        assert_eq!(std::fs::read(&database).unwrap(), before);
        // A missing liveness table reads as no heartbeat, not an error.
        let index = OwnedIndex::open_read_only(&database).unwrap();
        assert!(index.read_daemon_state().unwrap().is_none());
        // And any write through the handle fails instead of slipping in.
        let beat = crate::daemon_state::DaemonBeat {
            heartbeat_at_ns: 1,
            activity: crate::daemon_state::DaemonActivity::Idle,
            current_root: None,
            current_path: None,
            queue: crate::daemon_state::DaemonQueue::default(),
        };
        assert!(
            index.publish_daemon_state(1, 1, &beat).is_err(),
            "query_only must refuse writes"
        );
    }

    #[test]
    fn read_only_open_rejects_an_unknown_schema() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        OwnedIndex::open(&database).unwrap();
        Connection::open(&database)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();

        let result = OwnedIndex::open_read_only(&database);

        assert!(matches!(
            result,
            Err(OwnedIndexError::UnsupportedSchema(99))
        ));
    }

    #[test]
    fn read_only_open_carries_the_same_busy_timeout_as_writable_opens() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        OwnedIndex::open(&database).unwrap();
        let index = OwnedIndex::open_read_only(&database).unwrap();

        let busy_timeout: i64 = index
            .connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();

        // Without it contended reads fail instantly with BUSY; old reads
        // went through open() with 5s.
        assert_eq!(busy_timeout, 5_000);
    }

    #[test]
    fn a_read_only_open_waits_out_a_contended_lock_instead_of_failing_busy() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        OwnedIndex::open(&database).unwrap();
        // An EXCLUSIVE lock blocks even readers: the open below (and its
        // version probe) must wait for the release instead of failing.
        let blocker = Connection::open(&database).unwrap();
        blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            blocker.execute_batch("COMMIT").unwrap();
        });

        let index = OwnedIndex::open_read_only(&database).unwrap();
        let roots = index.roots().unwrap();

        releaser.join().unwrap();
        assert!(roots.is_empty());
    }

    #[test]
    fn read_only_inspection_reads_a_version_1_database_without_migrating() {
        use crate::providers::OwnedIndexSearchProvider;
        use crate::search::{MediaType, SearchFields, SearchProvider, SearchRequest, PAGE_SIZE};
        use crate::search_log::SearchLogContext;

        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("family-trip.mp4"), b"video").unwrap();
        {
            let mut index = OwnedIndex::open(&database).unwrap();
            index.insert_root("root", &root).unwrap();
            index.scan_root("root", &root, |_| {}).unwrap();
            // Rewind to version 1: no liveness table, no indexable
            // counter, version markers back to 1.
            index
                .connection
                .execute_batch(
                    "DROP TABLE daemon_state;
                     ALTER TABLE scan_runs DROP COLUMN files_indexable;
                     UPDATE schema_meta SET value = 1 WHERE key = 'schema_version';",
                )
                .unwrap();
            index
                .connection
                .pragma_update(None, "user_version", 1)
                .unwrap();
        }
        let before = std::fs::read(&database).unwrap();

        // Everything `ts` reads: folders, stats, and search.
        let manager = OwnedIndexManager::under_read_only(database.clone());
        let folders = manager.folders().unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].status, OwnedFolderStatus::Ready);
        let stats = manager.stats().unwrap();
        assert_eq!(stats.videos, 1);
        assert_eq!(stats.folders.len(), 1);
        let provider = OwnedIndexSearchProvider::new_read_only(database.clone());
        let request = SearchRequest {
            query: "family".into(),
            page: 1,
            page_size: PAGE_SIZE,
            search_id: None,
            fields: SearchFields::default(),
            media_type: MediaType::Videos,
        };
        let context = SearchLogContext::from(&request);
        let candidates = provider.candidates("family", false, &context).unwrap();
        assert_eq!(candidates, vec![root.join("family-trip.mp4")]);
        assert!(provider.revision() > 0);
        let mut paths = vec![
            root.join("family-trip.mp4"),
            PathBuf::from("/elsewhere/nope.mp4"),
        ];
        provider.retain_visible_paths(&mut paths).unwrap();
        assert_eq!(paths, vec![root.join("family-trip.mp4")]);

        // Still version 1, byte for byte: no read migrated anything.
        let version: i64 = Connection::open(&database)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }

    #[test]
    fn scan_progress_on_version_1_reports_committed_files_as_indexable() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("family-trip.mp4"), b"video").unwrap();
        std::fs::write(root.join("notes.txt"), b"unsupported").unwrap();
        {
            let mut index = OwnedIndex::open(&database).unwrap();
            index.insert_root("root", &root).unwrap();
            index.scan_root("root", &root, |_| {}).unwrap();
            index
                .connection
                .execute_batch(
                    "DROP TABLE daemon_state;
                     ALTER TABLE scan_runs DROP COLUMN files_indexable;
                     UPDATE schema_meta SET value = 1 WHERE key = 'schema_version';",
                )
                .unwrap();
            index
                .connection
                .pragma_update(None, "user_version", 1)
                .unwrap();
        }

        let index = OwnedIndex::open_read_only(&database).unwrap();
        let progress = index.scan_progress("root").unwrap().expect("scan run");

        assert_eq!(progress.phase, ScanPhase::Ready);
        assert_eq!(progress.files_discovered, 2);
        assert_eq!(progress.files_indexed, 1);
        // No indexable counter on version 1: the committed count stands
        // in, so remaining work reads as none.
        assert_eq!(progress.files_indexable, 1);
    }

    #[test]
    fn read_only_inspection_of_an_empty_database_is_empty() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        // Version 0: an empty file with no tables at all.
        std::fs::write(&database, []).unwrap();

        let manager = OwnedIndexManager::under_read_only(database.clone());
        assert!(manager.folders().unwrap().is_empty());
        let stats = manager.stats().unwrap();
        assert_eq!(stats.revision, 0);
        assert_eq!(stats.videos, 0);
        assert_eq!(stats.images, 0);
        assert!(stats.folders.is_empty());

        let index = OwnedIndex::open_read_only(&database).unwrap();
        assert!(index
            .search_candidates(&["family".to_owned()])
            .unwrap()
            .is_empty());
        assert_eq!(index.revision().unwrap(), 0);
        assert!(index.roots().unwrap().is_empty());
        assert!(index.scan_progress("root").unwrap().is_none());

        // And nothing was created: still an empty file.
        assert_eq!(std::fs::read(&database).unwrap().len(), 0);
    }

    #[test]
    fn writes_through_a_read_only_manager_fail_loudly() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("state").join("index.sqlite3");
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        OwnedIndexManager::under(database.clone())
            .add_folder(&root)
            .unwrap();
        let before = std::fs::read(&database).unwrap();

        let manager = OwnedIndexManager::under_read_only(database.clone());
        let other = directory.path().join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(manager.add_folder(&other).is_err());
        assert!(manager.remove_folder("root").is_err());

        assert_eq!(std::fs::read(&database).unwrap(), before);
    }

    #[test]
    fn concurrent_first_opens_migrate_exactly_once() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        {
            let mut index = OwnedIndex::open(&database).unwrap();
            index.insert_root("root", &root).unwrap();
            index.scan_root("root", &root, |_| {}).unwrap();
            index
                .connection
                .execute_batch(
                    "DROP TABLE daemon_state;
                     ALTER TABLE scan_runs DROP COLUMN files_indexable;
                     UPDATE schema_meta SET value = 1 WHERE key = 'schema_version';",
                )
                .unwrap();
            index
                .connection
                .pragma_update(None, "user_version", 1)
                .unwrap();
        }

        // Eight first-openers race the same version 1 database: one
        // migrates, the rest wait out the busy timeout and find nothing
        // left to do. Every opener succeeds.
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let database = database.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    OwnedIndex::open(&database)
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        let index = OwnedIndex::open(&database).unwrap();
        let version: i64 = index
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        assert_eq!(index.roots().unwrap().len(), 1);
        assert_eq!(indexed_display_paths(&index).len(), 1);

        // The same race for fresh creation: one wins, the rest open what
        // the winner created.
        let fresh = directory.path().join("fresh.sqlite3");
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let fresh = fresh.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    OwnedIndex::open(&fresh)
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        let version: i64 = Connection::open(&fresh)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    #[test]
    fn write_transactions_hold_the_write_lock_up_front() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut first = OwnedIndex::open(&database).unwrap();

        let transaction = write_transaction(&mut first.connection).unwrap();

        // A second writer cannot even begin while the first holds its
        // transaction open with no statements run: the lock was taken up
        // front, not deferred to the first write — which is what keeps a
        // concurrent commit from failing the upgrade with BUSY_SNAPSHOT.
        let second = Connection::open(&database).unwrap();
        second.busy_timeout(Duration::ZERO).unwrap();
        let blocked = second.execute_batch("BEGIN IMMEDIATE");
        assert!(
            matches!(
                blocked,
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::DatabaseBusy
            ),
            "expected BUSY, got {blocked:?}"
        );
        drop(transaction);
        second.execute_batch("BEGIN IMMEDIATE; COMMIT;").unwrap();
    }

    #[test]
    fn migrating_preserves_scan_error_and_offline_roots_with_wal_present() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        for name in ["scanning", "errored", "offline"] {
            std::fs::create_dir(directory.path().join(name)).unwrap();
        }
        let offline_dir = directory.path().join("offline").canonicalize().unwrap();
        std::fs::write(offline_dir.join("clip.mp4"), b"video").unwrap();
        let mut index = OwnedIndex::open(&database).unwrap();
        index
            .insert_root("scanning", &directory.path().join("scanning"))
            .unwrap();
        index
            .insert_root("errored", &directory.path().join("errored"))
            .unwrap();
        index.insert_root("offline", &offline_dir).unwrap();
        index.scan_root("offline", &offline_dir, |_| {}).unwrap();
        // A mid-scan run and an errored run, as an interrupted daemon
        // would leave them, plus the scanned root going offline — then
        // the whole thing rewound to version 1.
        index
            .connection
            .execute_batch(
                "UPDATE roots SET state = 'scanning', scan_generation = 1 WHERE id = 'scanning';
                 INSERT INTO scan_runs (
                    root_id, generation, state,
                    folders_discovered, folders_scanned,
                    files_discovered, files_indexable, files_indexed,
                    videos_indexed, images_indexed,
                    started_at_ns, updated_at_ns
                 ) VALUES ('scanning', 1, 'scanning', 4, 2, 10, 8, 5, 5, 0, 1, 2);
                 UPDATE roots SET state = 'error', scan_generation = 1 WHERE id = 'errored';
                 INSERT INTO scan_runs (
                    root_id, generation, state,
                    folders_discovered, folders_scanned,
                    files_discovered, files_indexable, files_indexed,
                    videos_indexed, images_indexed,
                    started_at_ns, updated_at_ns, error
                 ) VALUES ('errored', 1, 'error', 2, 2, 6, 6, 6, 5, 1, 1, 2, 'boom');
                 UPDATE roots SET state = 'offline' WHERE id = 'offline';
                 DROP TABLE daemon_state;
                 ALTER TABLE scan_runs DROP COLUMN files_indexable;
                 UPDATE schema_meta SET value = 1 WHERE key = 'schema_version';",
            )
            .unwrap();
        index
            .connection
            .pragma_update(None, "user_version", 1)
            .unwrap();
        // Uncheckpointed version 1 content still sits in the WAL, and the
        // old handle stays open while the migration runs — as a version 1
        // app holding the database would — so the migration must read
        // through the WAL instead of a clean file.
        let wal = database.with_extension("sqlite3-wal");
        assert!(
            std::fs::metadata(&wal).unwrap().len() > 0,
            "expected WAL content beside the version 1 database"
        );
        let migrated = OwnedIndex::open(&database).unwrap();
        drop(index);
        let index = migrated;

        let version: i64 = index
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 2);
        let states = index.roots_with_state().unwrap();
        assert_eq!(
            states
                .iter()
                .map(|(id, _, state)| (id.as_str(), state.as_str()))
                .collect::<Vec<_>>(),
            [
                ("errored", "error"),
                ("offline", "offline"),
                ("scanning", "scanning")
            ]
        );
        let mid = index
            .scan_progress("scanning")
            .unwrap()
            .expect("mid-scan run");
        assert_eq!(mid.phase, ScanPhase::Scanning);
        assert_eq!(mid.files_discovered, 10);
        assert_eq!(mid.files_indexed, 5);
        // The re-added counter defaults to zero for pre-migration rows.
        assert_eq!(mid.files_indexable, 0);
        let failed = index
            .scan_progress("errored")
            .unwrap()
            .expect("errored run");
        assert_eq!(failed.phase, ScanPhase::Error);
        assert_eq!(failed.error.as_deref(), Some("boom"));
        // The offline root's files survive the migration.
        assert_eq!(indexed_display_paths(&index).len(), 1);
        assert!(index.read_daemon_state().unwrap().is_none());
    }

    #[test]
    fn opening_a_newer_schema_fails_cleanly_without_migrating() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        OwnedIndex::open(&database).unwrap();
        Connection::open(&database)
            .unwrap()
            .pragma_update(None, "user_version", 999)
            .unwrap();
        // Roll back to delete-mode with loose permissions, so any chmod
        // or WAL flip during the refusal below shows up.
        Connection::open(&database)
            .unwrap()
            .execute_batch("PRAGMA journal_mode = DELETE;")
            .unwrap();
        let sidecars = ["-wal", "-shm", "-journal"]
            .map(|suffix| PathBuf::from(format!("{}{suffix}", database.display())));
        for sidecar in &sidecars {
            let _ = std::fs::remove_file(sidecar);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o644)).unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let before = std::fs::read(&database).unwrap();
        #[cfg(unix)]
        let mode_before = {
            use std::os::unix::fs::PermissionsExt;
            (
                std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
                std::fs::metadata(directory.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
            )
        };

        let result = OwnedIndex::open(&database);

        // A downgrade is a clean error, not a migration attempt: the
        // version check runs before any side effect, so the database —
        // bytes, mode, and sidecars — is untouched and still opens once
        // upgraded again.
        assert!(matches!(
            result,
            Err(OwnedIndexError::UnsupportedSchema(999))
        ));
        assert_eq!(std::fs::read(&database).unwrap(), before);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                (
                    std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
                    std::fs::metadata(directory.path())
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                ),
                mode_before
            );
        }
        for sidecar in &sidecars {
            assert!(
                !sidecar.exists(),
                "refusal must not create {}",
                sidecar.display()
            );
        }
        Connection::open(&database)
            .unwrap()
            .pragma_update(None, "user_version", 2)
            .unwrap();
        OwnedIndex::open(&database).unwrap();
    }

    fn dir_listing(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn refusing_a_newer_wal_database_touches_neither_bytes_nor_sidecars() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        OwnedIndex::open(&database).unwrap();
        // A newer app's index: version 999, WAL mode with live sidecars.
        // Refusal happens from raw header bytes before any open, so it
        // cannot create -shm, roll back a hot journal, or touch modes —
        // by construction, not just by ordering inside open().
        Connection::open(&database)
            .unwrap()
            .pragma_update(None, "user_version", 999)
            .unwrap();
        let held = Connection::open(&database).unwrap();
        held.execute_batch("CREATE TABLE probe(x); INSERT INTO probe VALUES (1);")
            .unwrap();
        let before = std::fs::read(&database).unwrap();
        let wal_before =
            std::fs::read(PathBuf::from(format!("{}-wal", database.display()))).unwrap();
        let listing_before = dir_listing(directory.path());

        let result = OwnedIndex::open(&database);

        assert!(matches!(
            result,
            Err(OwnedIndexError::UnsupportedSchema(999))
        ));
        assert_eq!(std::fs::read(&database).unwrap(), before);
        assert_eq!(
            std::fs::read(PathBuf::from(format!("{}-wal", database.display()))).unwrap(),
            wal_before
        );
        assert_eq!(dir_listing(directory.path()), listing_before);
        drop(held);
    }

    #[test]
    fn open_retries_the_wal_flip_past_a_reserved_lock() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        OwnedIndex::open(&database).unwrap();
        // RESERVED lets the version probe read but blocks the WAL flip's
        // EXCLUSIVE lock — and PRAGMA bypasses busy_timeout, so without
        // the application-level retry this open fails instantly with BUSY.
        let blocker = Connection::open(&database).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            blocker.execute_batch("COMMIT").unwrap();
        });

        OwnedIndex::open(&database).unwrap();

        releaser.join().unwrap();
    }

    #[test]
    fn entering_repair_clears_the_last_reported_progress() {
        let shared = WorkerShared::new();
        let mut progress = ScanProgress::new("root");
        progress.current_file = Some(PathBuf::from("/media/a.mp4"));
        shared.record_progress(&progress);
        shared.set_stage(WorkerStage::Scanning);
        assert!(shared.snapshot().last_progress.is_some());

        // Entering repair drops the scan's stale path...
        shared.set_stage(WorkerStage::Repairing);
        let live = shared.snapshot();
        assert_eq!(live.stage, WorkerStage::Repairing);
        assert!(live.last_progress.is_none());

        // ...until the repair itself reports progress.
        shared.record_progress(&progress);
        assert!(shared.snapshot().last_progress.is_some());

        // Other transitions keep what was reported.
        shared.set_stage(WorkerStage::Watching);
        assert!(shared.snapshot().last_progress.is_some());
    }

    #[test]
    fn negative_stored_counts_read_as_zero() {
        let (_directory, index) = open_index();
        let beat = crate::daemon_state::DaemonBeat {
            heartbeat_at_ns: 2_000_000_000,
            activity: crate::daemon_state::DaemonActivity::Idle,
            current_root: None,
            current_path: None,
            queue: crate::daemon_state::DaemonQueue::default(),
        };
        index
            .publish_daemon_state(1234, 1_000_000_000, &beat)
            .unwrap();
        index
            .connection
            .execute(
                "UPDATE daemon_state
                 SET pending_roots = -5, files_remaining = -3,
                     pending_repairs = -1, pending_thumbnails = -7
                 WHERE id = 1",
                [],
            )
            .unwrap();

        let row = index.read_daemon_state().unwrap().expect("heartbeat row");

        assert_eq!(row.queue, crate::daemon_state::DaemonQueue::default());
    }

    #[test]
    fn daemon_heartbeat_round_trips_through_the_liveness_row() {
        let (_directory, index) = open_index();
        assert!(index.read_daemon_state().unwrap().is_none());

        let queue = crate::daemon_state::DaemonQueue {
            pending_roots: 2,
            files_remaining: 40,
            pending_repairs: 1,
            pending_thumbnails: 3,
        };
        let beat = crate::daemon_state::DaemonBeat {
            heartbeat_at_ns: 2_000_000_000,
            activity: crate::daemon_state::DaemonActivity::Scanning,
            current_root: Some(PathBuf::from("/media")),
            current_path: Some(PathBuf::from("/media/a.mp4")),
            queue: queue.clone(),
        };
        index
            .publish_daemon_state(1234, 1_000_000_000, &beat)
            .unwrap();
        let row = index.read_daemon_state().unwrap().expect("heartbeat row");
        assert_eq!(row.pid, Some(1234));
        assert_eq!(row.started_at_ns, 1_000_000_000);
        assert_eq!(row.heartbeat_at_ns, 2_000_000_000);
        assert_eq!(row.activity, crate::daemon_state::DaemonActivity::Scanning);
        assert_eq!(row.current_root, Some(PathBuf::from("/media")));
        assert_eq!(row.current_path, Some(PathBuf::from("/media/a.mp4")));
        assert_eq!(row.queue, queue);

        // A second heartbeat replaces the row instead of adding one.
        let beat = crate::daemon_state::DaemonBeat {
            heartbeat_at_ns: 3_000_000_000,
            activity: crate::daemon_state::DaemonActivity::Idle,
            current_root: None,
            current_path: None,
            queue: crate::daemon_state::DaemonQueue::default(),
        };
        index
            .publish_daemon_state(1234, 1_000_000_000, &beat)
            .unwrap();
        let row = index.read_daemon_state().unwrap().expect("heartbeat row");
        assert_eq!(row.heartbeat_at_ns, 3_000_000_000);
        assert_eq!(row.activity, crate::daemon_state::DaemonActivity::Idle);
        assert_eq!(row.current_root, None);
        let count: i64 = index
            .connection
            .query_row("SELECT COUNT(*) FROM daemon_state", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn daemon_queue_counts_pending_roots_files_and_repairs() {
        let directory = tempdir().unwrap();
        let ready_root = directory.path().join("ready");
        let busy_root = directory.path().join("busy");
        std::fs::create_dir(&ready_root).unwrap();
        std::fs::create_dir(&busy_root).unwrap();
        let ready_root = ready_root.canonicalize().unwrap();
        let busy_root = busy_root.canonicalize().unwrap();
        std::fs::write(ready_root.join("clip.mp4"), b"video").unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("ready", &ready_root).unwrap();
        index.scan_root("ready", &ready_root, |_| {}).unwrap();
        index.insert_root("busy", &busy_root).unwrap();
        // A scan generation in progress: fifty files seen, forty of them
        // indexable, thirty indexed. The ten excluded files (unsupported,
        // empty, unreadable) are decided, not remaining.
        index
            .connection
            .execute(
                "UPDATE roots SET state = 'scanning', scan_generation = 1 WHERE id = 'busy'",
                [],
            )
            .unwrap();
        index
            .connection
            .execute(
                "INSERT INTO scan_runs (
                    root_id, generation, state, folders_discovered,
                    files_discovered, files_indexable, files_indexed,
                    started_at_ns, updated_at_ns
                 ) VALUES ('busy', 1, 'scanning', 3, 50, 40, 30, 0, 0)",
                [],
            )
            .unwrap();
        index
            .queue_repair(&busy_root.join("late.mp4"), "direct update", "boom")
            .unwrap();

        let queue = index.daemon_queue_counts(7).unwrap();

        assert_eq!(queue.pending_roots, 1);
        assert_eq!(queue.files_remaining, 10);
        assert_eq!(queue.pending_repairs, 1);
        assert_eq!(queue.pending_thumbnails, 7);

        // A superseded generation's stale 'scanning' row does not count.
        index
            .connection
            .execute(
                "INSERT INTO scan_runs (
                    root_id, generation, state, folders_discovered,
                    files_discovered, files_indexable, files_indexed,
                    started_at_ns, updated_at_ns
                 ) VALUES ('busy', 0, 'scanning', 1, 1000, 900, 0, 0, 0)",
                [],
            )
            .unwrap();
        assert_eq!(index.daemon_queue_counts(0).unwrap().files_remaining, 10);
    }

    #[test]
    fn files_remaining_clamps_over_committed_runs_before_summing() {
        let directory = tempdir().unwrap();
        let migrated_root = directory.path().join("migrated");
        let busy_root = directory.path().join("busy");
        std::fs::create_dir(&migrated_root).unwrap();
        std::fs::create_dir(&busy_root).unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("migrated", &migrated_root).unwrap();
        index.insert_root("busy", &busy_root).unwrap();
        // A version 1 scan interrupted mid-run migrates to indexable 0 /
        // indexed N: the new counter defaults to zero for pre-migration
        // rows while the committed count survives.
        index
            .connection
            .execute(
                "UPDATE roots SET state = 'scanning', scan_generation = 1 WHERE id = 'migrated'",
                [],
            )
            .unwrap();
        index
            .connection
            .execute(
                "INSERT INTO scan_runs (
                    root_id, generation, state, folders_discovered,
                    files_discovered, files_indexable, files_indexed,
                    started_at_ns, updated_at_ns
                 ) VALUES ('migrated', 1, 'scanning', 1, 12, 0, 12, 0, 0)",
                [],
            )
            .unwrap();
        // Beside a root with ten files seen but uncommitted: clamping
        // after the sum would report max(0, -12 + 10) = 0 and hide the
        // real remainder.
        index
            .connection
            .execute(
                "UPDATE roots SET state = 'scanning', scan_generation = 1 WHERE id = 'busy'",
                [],
            )
            .unwrap();
        index
            .connection
            .execute(
                "INSERT INTO scan_runs (
                    root_id, generation, state, folders_discovered,
                    files_discovered, files_indexable, files_indexed,
                    started_at_ns, updated_at_ns
                 ) VALUES ('busy', 1, 'scanning', 1, 10, 10, 0, 0, 0)",
                [],
            )
            .unwrap();

        assert_eq!(index.daemon_queue_counts(0).unwrap().files_remaining, 10);
    }

    #[test]
    fn files_remaining_counts_only_uncommitted_indexable_files() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("first.mp4"), b"one").unwrap();
        std::fs::write(root.join("second.mp4"), b"two").unwrap();
        std::fs::write(root.join("notes.txt"), b"text").unwrap();
        std::fs::File::create(root.join("empty.mov")).unwrap();
        // An unread directory behind the files guarantees a cancellation
        // check still waits after every file is classified, whatever
        // order the filesystem lists them in: without it the scan could
        // commit its batch and finish before noticing the flag.
        std::fs::create_dir(root.join("later")).unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut scanner = OwnedIndex::open(&database).unwrap();
        scanner.insert_root("root", &root).unwrap();
        let observer = OwnedIndex::open(&database).unwrap();

        // Abort once every file is classified but no batch committed: the
        // notes and the empty clip are excluded, never remaining.
        let cancel = AtomicBool::new(false);
        let result =
            scanner.scan_root_in_batches("root", &root, SCAN_BATCH_SIZE, &cancel, |progress| {
                if progress.files_discovered == 4 {
                    cancel.store(true, Ordering::Relaxed);
                }
            });
        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );

        let stored = observer.scan_progress("root").unwrap().unwrap();
        assert_eq!(stored.phase, ScanPhase::Scanning);
        assert_eq!(stored.files_discovered, 4);
        assert_eq!(stored.files_indexable, 2);
        assert_eq!(stored.files_indexed, 0);
        assert_eq!(observer.daemon_queue_counts(0).unwrap().files_remaining, 2);
    }

    #[test]
    fn a_cancelled_scan_is_not_recorded_as_successful() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("clip.mp4"), b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();

        let cancel = AtomicBool::new(true);
        let error = index
            .scan_root_in_batches("root", &root, SCAN_BATCH_SIZE, &cancel, |_| {})
            .unwrap_err();
        assert!(
            matches!(error, OwnedIndexError::Cancelled { .. }),
            "expected Cancelled, got {error}"
        );

        // The roots row and the run stay 'scanning': nothing claims the
        // aborted scan finished.
        assert_eq!(index.roots_with_state().unwrap()[0].2, "scanning");
        let progress = index.scan_progress("root").unwrap().expect("partial run");
        assert_eq!(progress.phase, ScanPhase::Scanning);

        // A fresh scan completes honestly afterwards.
        let cancel = AtomicBool::new(false);
        let progress = index
            .scan_root_in_batches("root", &root, SCAN_BATCH_SIZE, &cancel, |_| {})
            .unwrap();
        assert_eq!(progress.phase, ScanPhase::Ready);
        assert_eq!(index.roots_with_state().unwrap()[0].2, "ready");
        assert_eq!(indexed_display_paths(&index).len(), 1);
    }

    #[test]
    fn cancelling_mid_scan_keeps_finished_writes_and_rescans_cleanly() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("clip-a.mp4"), b"video").unwrap();
        std::fs::write(root.join("clip-b.mp4"), b"video").unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();

        // Abort right after the first batch commits: the write must stand
        // while the scan itself must not count as finished.
        let cancel = AtomicBool::new(false);
        let mut indexed = 0u64;
        let result = index.scan_root_in_batches("root", &root, 1, &cancel, |progress| {
            if progress.files_indexed > indexed {
                indexed = progress.files_indexed;
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );

        assert_eq!(indexed, 1);
        assert_eq!(index.roots_with_state().unwrap()[0].2, "scanning");
        assert_eq!(indexed_display_paths(&index).len(), 1);

        let cancel = AtomicBool::new(false);
        let progress = index
            .scan_root_in_batches("root", &root, SCAN_BATCH_SIZE, &cancel, |_| {})
            .unwrap();
        assert_eq!(progress.phase, ScanPhase::Ready);
        assert_eq!(indexed_display_paths(&index).len(), 2);
    }

    #[test]
    fn a_cancelled_directory_sync_indexes_nothing_and_queues_its_repair() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let root = root.canonicalize().unwrap();
        let nested = root.join("nested");
        std::fs::write(nested.join("clip.mp4"), b"video").unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
        let cancel = AtomicBool::new(true);

        let result = index.synchronize_path("root", &root, &nested, &cancel);

        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        assert!(indexed_display_paths(&index).is_empty());
        // The unfinished update is queued for the next repair run.
        let repairs = index.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, nested);
    }

    #[test]
    fn a_cancelled_rename_sync_applies_nothing() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let from = root.join("clip.mp4");
        let to = root.join("renamed.mp4");
        std::fs::write(&from, b"video").unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        std::fs::rename(&from, &to).unwrap();
        let cancel = AtomicBool::new(true);

        let result = index.synchronize_rename("root", &root, &from, &to, &cancel);

        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        assert_eq!(
            indexed_display_paths(&index),
            BTreeSet::from([from.to_string_lossy().into_owned()])
        );
    }

    /// A root with `files` clips indexed under `old/`, then renamed on
    /// disk to `new/`: the rename sync below must move the rows. Every
    /// indexed file sits under the renamed directory, so every planning
    /// row matches.
    fn indexed_renamed_directory(
        files: usize,
    ) -> (tempfile::TempDir, OwnedIndex, PathBuf, PathBuf, PathBuf) {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let from = root.join("old");
        std::fs::create_dir_all(&from).unwrap();
        for number in 0..files {
            std::fs::write(from.join(format!("clip-{number:02}.mp4")), b"video").unwrap();
        }
        let root = root.canonicalize().unwrap();
        let from = root.join("old");
        let to = root.join("new");
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
        index.scan_root("root", &root, |_| {}).unwrap();
        std::fs::rename(&from, &to).unwrap();
        (directory, index, root, from, to)
    }

    fn index_revision(index: &OwnedIndex) -> i64 {
        index
            .connection
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'index_revision'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn a_rename_cancelled_mid_planning_commits_nothing_and_stays_queued() {
        let (_directory, mut index, root, from, to) = indexed_renamed_directory(3);
        let before = indexed_display_paths(&index);
        assert_eq!(before.len(), 3);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        let planned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&planned);

        // The stop arrives while planning is still reading rows: the
        // first planned row raises it, the next row's check honors it —
        // long before the rewrite transaction opens. The count proves
        // the per-row check fired, not just the post-planning one.
        let result = with_sync_step_probe(
            move |site| {
                if site == SyncProbeSite::RenamePlanned {
                    counted.fetch_add(1, Ordering::Relaxed);
                    stop.store(true, Ordering::Relaxed);
                }
            },
            || index.synchronize_rename("root", &root, &from, &to, &cancel),
        );

        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        assert_eq!(planned.load(Ordering::Relaxed), 1);
        assert_eq!(indexed_display_paths(&index), before);
        let repairs = index.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, to);
        assert_eq!(repairs[0].reason.as_str(), "direct rename");
    }

    #[test]
    fn a_rename_cancelled_after_planning_never_opens_its_rewrite() {
        let (_directory, mut index, root, from, to) = indexed_renamed_directory(3);
        let before = indexed_display_paths(&index);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        let planned = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&planned);
        let rewritten = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rewritten_counted = Arc::clone(&rewritten);

        // The stop lands on the last planning row: planning runs to
        // completion, but the rewrite transaction never opens.
        let result = with_sync_step_probe(
            move |site| match site {
                SyncProbeSite::RenamePlanned => {
                    if counted.fetch_add(1, Ordering::Relaxed) + 1 == 3 {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                SyncProbeSite::RenameRewritten => {
                    rewritten_counted.fetch_add(1, Ordering::Relaxed);
                }
                SyncProbeSite::FileCommitted | SyncProbeSite::ScanBatchCommitted => {}
            },
            || index.synchronize_rename("root", &root, &from, &to, &cancel),
        );

        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        assert_eq!(planned.load(Ordering::Relaxed), 3);
        assert_eq!(
            rewritten.load(Ordering::Relaxed),
            0,
            "the rewrite transaction must never open"
        );
        assert_eq!(indexed_display_paths(&index), before);
        let repairs = index.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, to);
    }

    #[test]
    fn a_rename_cancelled_on_its_only_row_rolls_back_before_commit() {
        let (_directory, mut index, root, from, to) = indexed_renamed_directory(1);
        let before = indexed_display_paths(&index);
        let revision = index_revision(&index);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);

        // A single moved file: the stop lands on its only rewrite, past
        // every per-row check, so only the pre-commit recheck can still
        // roll the transaction back.
        let result = with_sync_step_probe(
            move |site| {
                if site == SyncProbeSite::RenameRewritten {
                    stop.store(true, Ordering::Relaxed);
                }
            },
            || index.synchronize_rename("root", &root, &from, &to, &cancel),
        );

        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        assert_eq!(indexed_display_paths(&index), before);
        assert_eq!(
            index_revision(&index),
            revision,
            "the revision bump must roll back with the rewrite"
        );
        let repairs = index.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, to);
    }

    #[test]
    fn a_rename_cancelled_mid_rewrite_rolls_everything_back() {
        let (_directory, mut index, root, from, to) = indexed_renamed_directory(3);
        let before = indexed_display_paths(&index);
        let revision = index_revision(&index);
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        let rewritten = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = Arc::clone(&rewritten);

        // The stop arrives with the first rewrite: the second row's check
        // fires, and dropping the uncommitted transaction rolls back even
        // the first row's rewrite.
        let result = with_sync_step_probe(
            move |site| {
                if site == SyncProbeSite::RenameRewritten {
                    counted.fetch_add(1, Ordering::Relaxed);
                    stop.store(true, Ordering::Relaxed);
                }
            },
            || index.synchronize_rename("root", &root, &from, &to, &cancel),
        );

        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        assert_eq!(rewritten.load(Ordering::Relaxed), 1);
        assert_eq!(indexed_display_paths(&index), before);
        assert_eq!(
            index_revision(&index),
            revision,
            "the revision bump must roll back with the rewrite"
        );
        let repairs = index.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, to);
    }

    #[test]
    fn a_directory_sync_aborts_mid_walk_when_cancelled() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let root = root.canonicalize().unwrap();
        let nested = root.join("nested");
        const CLIPS: usize = 1200;
        for number in 0..CLIPS {
            std::fs::write(nested.join(format!("clip-{number:04}.mp4")), b"video").unwrap();
        }
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
        let observer = OwnedIndex::open(&database).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));

        // A bounded handshake at the first file commit: each file commits
        // its own transaction, so the worker signals and waits there while
        // the test sets the stop and releases it. No polling, so a
        // descheduled observer cannot miss the window — and a worker that
        // fails first reports its own failure instead of hanging the wait.
        let (committed_sender, committed_receiver) = mpsc::channel::<()>();
        let (release_sender, release_receiver) = mpsc::channel::<()>();
        let signalled = Arc::new(AtomicBool::new(false));
        let worker_signalled = Arc::clone(&signalled);
        let worker_cancel = Arc::clone(&cancel);
        let worker = thread::spawn(move || {
            with_sync_step_probe(
                move |site| {
                    if site == SyncProbeSite::FileCommitted
                        && !worker_signalled.swap(true, Ordering::Relaxed)
                    {
                        committed_sender.send(()).unwrap();
                        release_receiver
                            .recv_timeout(Duration::from_secs(30))
                            .expect("the test releases the worker after setting the stop");
                    }
                },
                || index.synchronize_path("root", &root, &nested, &worker_cancel),
            )
        });
        if committed_receiver
            .recv_timeout(Duration::from_secs(30))
            .is_err()
        {
            if worker.is_finished() {
                match worker.join() {
                    Ok(result) => {
                        panic!("the sync worker finished before committing anything: {result:?}")
                    }
                    Err(_) => panic!("the sync worker panicked before committing anything"),
                }
            }
            panic!("timed out waiting for the first committed file");
        }
        cancel.store(true, Ordering::Relaxed);
        release_sender.send(()).unwrap();
        let result = worker.join().expect("sync thread panicked");

        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        // The stop landed before the second file: exactly the first
        // commit survived.
        assert_eq!(indexed_display_paths(&observer).len(), 1);
    }

    #[test]
    fn a_cancelled_directory_repair_replays_on_the_next_run() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let root = root.canonicalize().unwrap();
        let nested = root.join("nested");
        std::fs::write(nested.join("clip.mp4"), b"video").unwrap();
        let database = directory.path().join("index.sqlite3");
        let mut index = OwnedIndex::open(&database).unwrap();
        index.insert_root("root", &root).unwrap();
        index
            .queue_repair(&nested, "direct update", "boom")
            .unwrap();
        let cancel = AtomicBool::new(true);

        let result = index.repair_pending("root", &root, &cancel, |_| {});

        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        let repairs = index.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, nested);

        // The next run replays it once the stop request is gone.
        let cancel = AtomicBool::new(false);
        assert_eq!(
            index
                .repair_pending("root", &root, &cancel, |_| {})
                .unwrap(),
            1
        );
        assert!(index.pending_repairs().unwrap().is_empty());
        assert_eq!(indexed_display_paths(&index).len(), 1);
    }

    #[test]
    fn a_cancelled_rename_repair_replays_to_completion_on_the_next_run() {
        let (_directory, mut index, root, from, to) = indexed_renamed_directory(3);
        let before: BTreeSet<String> = (0..3)
            .map(|number| {
                from.join(format!("clip-{number:02}.mp4"))
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(indexed_display_paths(&index), before);
        let cancel = AtomicBool::new(true);

        let result = index.synchronize_rename("root", &root, &from, &to, &cancel);

        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        assert_eq!(indexed_display_paths(&index), before);
        let repairs = index.pending_repairs().unwrap();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].path, to);
        assert_eq!(repairs[0].reason, "direct rename");

        // The next run replays the rename repair to completion: the rows
        // move to their new names and the queue drains.
        let cancel = AtomicBool::new(false);
        assert_eq!(
            index
                .repair_pending("root", &root, &cancel, |_| {})
                .unwrap(),
            1
        );
        assert!(index.pending_repairs().unwrap().is_empty());
        let expected: BTreeSet<String> = (0..3)
            .map(|number| {
                to.join(format!("clip-{number:02}.mp4"))
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(indexed_display_paths(&index), expected);
    }

    #[test]
    fn a_cancelled_repair_stays_queued_for_the_next_run() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let mut index = OwnedIndex::open(&directory.path().join("index.sqlite3")).unwrap();
        index.insert_root("root", &root).unwrap();
        index
            .queue_repair(&root.join("late.mp4"), "direct update", "boom")
            .unwrap();

        let cancel = AtomicBool::new(true);
        let result = index.repair_pending("root", &root, &cancel, |_| {});
        assert!(
            matches!(result, Err(OwnedIndexError::Cancelled { .. })),
            "expected Cancelled, got {result:?}"
        );
        assert_eq!(index.pending_repairs().unwrap().len(), 1);
    }

    #[test]
    fn stopping_a_watcher_mid_scan_cancels_promptly() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };

        let directory = tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        for number in 0..2000 {
            let name = format!("clip-{number:05}-with-a-long-name-to-index-properly.mp4");
            std::fs::write(root.join(name), b"video").unwrap();
        }
        let database = directory.path().join("index.sqlite3");
        // A root id unique to this test: the gate below only ever trips
        // for it, so parallel tests never observe each other's probes.
        OwnedIndex::open(&database)
            .unwrap()
            .insert_root("cancel-probe-root", &root)
            .unwrap();

        // Gate the worker after its first batch commit instead of racing
        // a poll against the scan: stop lands mid-scan deterministically
        // on any machine speed.
        let (gated_tx, gated_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = std::sync::Mutex::new(release_rx);
        let batches = std::sync::Arc::new(AtomicUsize::new(0));
        let worker_batches = std::sync::Arc::clone(&batches);
        with_global_sync_probe(
            "cancel-probe-root",
            move |site| {
                if site == SyncProbeSite::ScanBatchCommitted
                    && worker_batches.fetch_add(1, Ordering::Relaxed) + 1 == 1
                {
                    gated_tx.send(()).unwrap();
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(60))
                        .expect("the test releases the gate once stop is underway");
                }
            },
            || {
                let watcher = OwnedIndexWatcher::start_with_progress(
                    &database,
                    "cancel-probe-root".into(),
                    root,
                    |_| {},
                )
                .unwrap();
                gated_rx
                    .recv_timeout(Duration::from_secs(60))
                    .expect("the scan must commit a first batch");

                // Stop from another thread: stop() joins the worker, and
                // the worker sits in the gate until released below.
                let (stopped_tx, stopped_rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let _ = stopped_tx.send(watcher.stop());
                });
                release_tx.send(()).unwrap();
                stopped_rx
                    .recv_timeout(Duration::from_secs(60))
                    .expect("stop must join promptly, not wait out the scan")
                    .unwrap();

                // Aborted mid-scan, never recorded successful: one batch
                // committed, the bulk of the walk unvisited.
                let index = OwnedIndex::open(&database).unwrap();
                assert_eq!(index.roots_with_state().unwrap()[0].2, "scanning");
                let videos = OwnedIndexManager::under(database.clone())
                    .state()
                    .unwrap()
                    .indexed_videos;
                assert!(
                    (128..2000).contains(&videos),
                    "expected a partial scan, found {videos} videos"
                );
                let progress = index
                    .scan_progress("cancel-probe-root")
                    .unwrap()
                    .expect("partial run");
                assert_eq!(progress.phase, ScanPhase::Scanning);
            },
        );
    }
}
