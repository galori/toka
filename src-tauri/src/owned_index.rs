use rusqlite::Connection;
use std::{fs, path::Path, time::Duration};

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
}

pub(crate) struct OwnedIndex {
    connection: Connection,
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

        Ok(Self { connection })
    }
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
