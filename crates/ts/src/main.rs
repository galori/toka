mod cli;

use std::sync::Arc;
#[cfg(not(feature = "fixtures"))]
use toka_search::providers::OwnedIndexSearchProvider;
use toka_search::{
    index_stats::IndexStats,
    owned_index::{self, OwnedIndexFolder, OwnedIndexManager},
    search::{SearchEngine, SearchError, SearchProvider},
    search_log::{SearchLogContext, SearchLogger},
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
        &mut stdout,
        &mut stderr,
    );
    if status != 0 {
        std::process::exit(status);
    }
}
