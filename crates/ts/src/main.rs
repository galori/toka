mod cli;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
#[cfg(not(feature = "fixtures"))]
use toka_search::providers::OwnedIndexSearchProvider;
use toka_search::{
    index_stats::IndexStats,
    owned_index::{
        self, IndexUpdates, OwnedIndexFolder, OwnedIndexManager, OwnedIndexState, OwnedIndexUpdates,
    },
    search::{is_supported_image, SearchEngine, SearchError, SearchProvider},
    search_log::{SearchLogContext, SearchLogger},
    thumbnails,
};

/// Fixture media for integration tests, replacing `TOKA_E2E_VIDEOS` for this
/// binary. Honored only in builds with the `fixtures` feature.
#[cfg(feature = "fixtures")]
struct FixtureSearchProvider;

#[cfg(feature = "fixtures")]
impl SearchProvider for FixtureSearchProvider {
    fn candidates(
        &self,
        _query: &str,
        _whole_path: bool,
        _context: &SearchLogContext,
    ) -> Result<Vec<std::path::PathBuf>, SearchError> {
        let paths = std::env::var_os("TS_FIXTURE_VIDEOS").ok_or_else(|| {
            SearchError::Provider("The integration-test videos were not configured.".into())
        })?;
        Ok(std::env::split_paths(&paths).collect())
    }
}

/// A provider that answers every search with no candidates and touches
/// nothing on disk, for `ts` runs against a missing index. Searching must
/// not create an index that does not exist yet, exactly as `stats` and
/// `folders list` already refuse to. Fixture builds never construct it:
/// they always search the configured fixture videos instead.
#[cfg_attr(feature = "fixtures", allow(dead_code))]
struct EmptyProvider;

impl SearchProvider for EmptyProvider {
    fn candidates(
        &self,
        _query: &str,
        _whole_path: bool,
        _context: &SearchLogContext,
    ) -> Result<Vec<std::path::PathBuf>, SearchError> {
        Ok(Vec::new())
    }
}

fn search_provider() -> Result<Arc<dyn SearchProvider>, String> {
    #[cfg(feature = "fixtures")]
    {
        Ok(Arc::new(FixtureSearchProvider))
    }
    #[cfg(not(feature = "fixtures"))]
    {
        Ok(Arc::new(OwnedIndexSearchProvider::new(
            owned_index::database_path()?,
        )))
    }
}

fn create_engine_with_logger(logger: SearchLogger) -> Result<SearchEngine, String> {
    // A missing index searches empty instead of being created: opening it
    // would write a database and locking the rename overlay would write a
    // lock file, and a read-only command must leave no state behind. No
    // overlay path either, so nothing is ever locked or persisted.
    #[cfg(not(feature = "fixtures"))]
    if !owned_index::database_path()?.is_file() {
        return Ok(SearchEngine::new_with_logger(
            Arc::new(EmptyProvider),
            Arc::new(logger),
        ));
    }
    let provider = search_provider()?;
    let rename_overlay_path = Some(owned_index::search_rename_overlay_path()?);
    Ok(SearchEngine::new_with_logger_and_rename_store(
        provider,
        Arc::new(logger),
        rename_overlay_path,
    ))
}

fn create_engine() -> Result<SearchEngine, String> {
    // A CLI run surfaces its own errors; nothing should outlive it in a log.
    create_engine_with_logger(SearchLogger::disabled())
}

fn create_verbose_engine() -> Result<SearchEngine, String> {
    create_engine_with_logger(SearchLogger::with_verbose_capture())
}

fn read_index_stats() -> Result<IndexStats, String> {
    OwnedIndexManager::for_inspection()?.stats()
}

fn read_folders() -> Result<Vec<OwnedIndexFolder>, String> {
    OwnedIndexManager::for_inspection()?.folders()
}

/// Adds a folder exactly as the app does: the manager inserts the `roots`
/// row and returns the resulting state. `ts` starts no watcher of its own;
/// on Linux the daemon's watch on the index directory fires on the write
/// and its next reconcile picks the folder up, and on macOS the running
/// app's own manager already watches what it added itself.
fn add_folder(path: &Path) -> Result<OwnedIndexState, String> {
    OwnedIndexManager::for_inspection()?.add_folder(path)
}

fn remove_folder(id: &str) -> Result<OwnedIndexState, String> {
    OwnedIndexManager::for_inspection()?.remove_folder(id)
}

/// Refreshes one path after it changed on disk, as the app does after a
/// rename, move, or delete. A missing index is a silent no-op, exactly as
/// in the app: syncing must never create an index on its own.
fn sync_path(path: &Path) -> Result<(), String> {
    IndexUpdates::synchronize_path(&OwnedIndexUpdates::system(), path)
        .map_err(|error| error.to_string())
}

fn sync_rename(from: &Path, to: &Path) -> Result<(), String> {
    IndexUpdates::synchronize_rename(&OwnedIndexUpdates::system(), from, to)
        .map_err(|error| error.to_string())
}

/// The thumbnail for one file, mirroring `SearchEngine::thumbnail_path`:
/// an image is its own thumbnail, anything else goes through the shared
/// generator. The engine resolves a result id first, which also proves the
/// file is one it knows; `ts` takes a raw path, so an image that is not
/// there falls through to generation and fails honestly instead of
/// echoing back a path to nothing.
fn make_thumbnail(path: &Path) -> Result<PathBuf, String> {
    if is_supported_image(path) && path.is_file() {
        return Ok(path.to_path_buf());
    }
    thumbnails::generate(path)
        .ok_or_else(|| format!("Toka could not make a thumbnail for {}.", path.display()))
}

/// The preview frames for one file, mirroring
/// `SearchEngine::preview_paths` with the same image fast path.
fn make_preview(path: &Path) -> Result<Vec<PathBuf>, String> {
    if is_supported_image(path) && path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }
    thumbnails::preview(path)
        .ok_or_else(|| format!("Toka could not make a preview for {}.", path.display()))
}

fn main() {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let status = cli::run_command(
        &arguments,
        create_engine,
        create_verbose_engine,
        read_index_stats,
        read_folders,
        add_folder,
        remove_folder,
        sync_path,
        sync_rename,
        make_thumbnail,
        make_preview,
        &mut stdout,
        &mut stderr,
    );
    if status != 0 {
        std::process::exit(status);
    }
}
