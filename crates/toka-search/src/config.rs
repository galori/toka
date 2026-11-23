//! The values that tie this crate to the Toka app: where its data, logs,
//! and caches live and what its indexer service is called.
//!
//! They are constants, not parameters, so the extraction stays
//! behavior-identical; a later phase can turn them into configuration if the
//! `ts` utility needs different ones.

/// Folder inside the platform data directory holding the search index.
pub const APP_DATA_DIR_NAME: &str = "toka";

/// The search index database file.
pub const INDEX_FILE_NAME: &str = "index.sqlite3";

/// Folder inside the platform log/data directories holding Toka's logs.
pub const APP_LOG_DIR_NAME: &str = "app.toka.desktop";

/// Folder inside the temporary directory holding generated thumbnails.
pub const THUMBNAIL_CACHE_DIR_NAME: &str = "toka-thumbnails";

/// The systemd user unit running the indexer daemon on Linux.
pub const INDEXER_SERVICE_NAME: &str = "toka-indexer.service";

/// The systemd user unit `ts install` writes on Linux. It replaces
/// [`INDEXER_SERVICE_NAME`], which `ts install` migrates away.
pub const TS_SERVICE_NAME: &str = "toka-search.service";

/// The launchd agent label `ts install` registers on macOS.
pub const TS_LAUNCHD_LABEL: &str = "app.toka.ts";

/// Filename prefix of the per-search verbose logs.
pub const VERBOSE_LOG_FILE_PREFIX: &str = "toka_search_log_";

/// Filename prefix of the probe file checking a verbose-log folder is writable.
pub const LOG_WRITE_PROBE_PREFIX: &str = ".toka-search-log-write-test-";
