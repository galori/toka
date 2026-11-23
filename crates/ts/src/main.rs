mod cli;

use std::sync::Arc;
use toka_search::{
    index_stats::IndexStats,
    owned_index::{self, OwnedIndexFolder, OwnedIndexManager},
    providers::OwnedIndexSearchProvider,
    search::{SearchEngine, SearchProvider},
    search_log::SearchLogger,
};
#[cfg(feature = "fixtures")]
use toka_search::{search::SearchError, search_log::SearchLogContext};

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

fn search_rename_overlay_path() -> Result<std::path::PathBuf, String> {
    Ok(owned_index::database_path()?.with_file_name("search-renames.json"))
}

fn create_engine_with_logger(logger: SearchLogger) -> Result<SearchEngine, String> {
    let provider = search_provider()?;
    let rename_overlay_path = Some(search_rename_overlay_path()?);
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
