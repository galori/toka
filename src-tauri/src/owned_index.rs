use crate::{
    search::{is_supported_image, is_supported_video},
    tags,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const SCHEMA_VERSION: i64 = 1;

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
    #[error("Toka could not secure {path}: {source}")]
    Permissions {
        path: String,
        #[source]
        source: std::io::Error,
    },
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
