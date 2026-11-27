mod cli;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
#[cfg(not(feature = "fixtures"))]
use toka_search::providers::OwnedIndexSearchProvider;
use toka_search::{
    daemon_state::DaemonStatus,
    index_stats::IndexStats,
    owned_index::{
        self, IndexUpdates, OwnedIndexFolder, OwnedIndexManager, OwnedIndexState,
        OwnedIndexUpdates, SyncOutcome,
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
    ) -> Result<toka_search::search::CandidateSet, SearchError> {
        let paths = std::env::var_os("TS_FIXTURE_VIDEOS").ok_or_else(|| {
            SearchError::Provider("The integration-test videos were not configured.".into())
        })?;
        Ok(toka_search::search::CandidateSet {
            paths: std::env::split_paths(&paths).collect(),
            durations: Default::default(),
        })
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
    ) -> Result<toka_search::search::CandidateSet, SearchError> {
        Ok(toka_search::search::CandidateSet::default())
    }
}

fn search_provider() -> Result<Arc<dyn SearchProvider>, String> {
    #[cfg(feature = "fixtures")]
    {
        Ok(Arc::new(FixtureSearchProvider))
    }
    #[cfg(not(feature = "fixtures"))]
    {
        Ok(Arc::new(OwnedIndexSearchProvider::new_read_only(
            owned_index::database_path()?,
        )))
    }
}

fn create_engine_with_logger(logger: SearchLogger) -> Result<SearchEngine, String> {
    // A missing index searches empty instead of being created: opening it
    // would write a database and locking the rename overlay would write a
    // lock file, and a read-only command must leave no state behind. No
    // overlay path either, so nothing is ever locked or persisted. Only a
    // truly missing index reads empty, though: any other metadata failure
    // (permissions, I/O) is an error, and a non-file at the index path is
    // a broken setup — `is_file` answers false for all three, which used
    // to report every metadata failure as a healthy-empty search.
    #[cfg(not(feature = "fixtures"))]
    {
        let database = owned_index::database_path()?;
        if !owned_index::index_file_for_inspection(&database)? {
            return Ok(SearchEngine::new_with_logger(
                Arc::new(EmptyProvider),
                Arc::new(logger),
            ));
        }
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
    OwnedIndexManager::for_read_only_inspection()?.stats()
}

fn read_status() -> Result<DaemonStatus, String> {
    toka_search::daemon_state::read_status(&owned_index::database_path()?)
}

/// Runs the indexer daemon in the foreground. This is the only entry
/// point that keeps the index fresh: the app supervises nothing itself.
fn run_daemon() -> Result<(), String> {
    toka_search::indexer_daemon::run_daemon()
}

/// Plans (or plans and applies) a reset of the search state: the same
/// index, log, and thumbnail targets `toka reset` removes, minus the
/// app's own caches — and, when confirmed, the same fail-closed stop of
/// the indexer services first.
fn plan_and_apply_reset(confirmed: bool) -> Result<cli::ResetEffect, String> {
    use toka_search::{reset, service};
    let mut plan = reset::ResetPlan::new(service::reset_data_paths()?);
    plan.stop_indexer = Some(service::stop_indexer_services);
    if !confirmed {
        return Ok(cli::ResetEffect::Preview(plan.targets));
    }
    Ok(cli::ResetEffect::Applied(reset::apply(&plan)))
}

/// Runs one service operation against the real machine. Only install
/// and uninstall need the executable resolved — the service file points
/// at it, and the link cleanup compares against it — so the other
/// operations use a query host, which an unresolvable current executable
/// cannot fail.
fn manage_service(
    op: toka_search::service::ServiceOp,
) -> Result<toka_search::service::Outcome, (String, toka_search::service::Outcome)> {
    use toka_search::service::{Outcome, ServiceOp, SystemHost};
    let mut host = match op {
        ServiceOp::Install { .. } | ServiceOp::Uninstall { .. } => {
            SystemHost::new().map_err(|error| (error, Outcome::default()))?
        }
        ServiceOp::Restart | ServiceOp::Start | ServiceOp::Stop => {
            SystemHost::for_query().map_err(|error| (error, Outcome::default()))?
        }
    };
    toka_search::service::perform(&mut host, op)
}

fn read_folders() -> Result<Vec<OwnedIndexFolder>, String> {
    OwnedIndexManager::for_read_only_inspection()?.folders()
}

/// Adds a folder exactly as the app does: the manager inserts the `roots`
/// row and returns the resulting state. `ts` starts no watcher of its own;
/// on Linux the daemon's watch on the index directory fires on the write
/// and its next reconcile picks the folder up, and on macOS the running
/// app's next `index_state` poll runs `ensure_watchers`, which starts
/// watching the `ts`-added folder too.
fn add_folder(path: &Path) -> Result<(OwnedIndexState, PathBuf), String> {
    OwnedIndexManager::for_inspection()?.add_folder_with_committed_path(path)
}

fn remove_folder(id: &str) -> Result<OwnedIndexState, String> {
    OwnedIndexManager::for_inspection()?.remove_folder(id)
}

/// Refreshes one path after it changed on disk, as the app does after a
/// rename, move, or delete. A missing index is a silent no-op, exactly as
/// in the app: syncing must never create an index on its own. Anything
/// else that matches no watched folder is an error, not a quiet skip.
fn sync_path(path: &Path) -> Result<(), String> {
    match IndexUpdates::synchronize_path(&OwnedIndexUpdates::system(), path) {
        Ok(SyncOutcome::Matched | SyncOutcome::NoIndex) => Ok(()),
        Ok(SyncOutcome::Unmatched) => {
            Err(format!("{} is not in any watched folder.", path.display()))
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Moves one indexed path to its new name. Both sides must sit under the
/// same watched folder; anything else is an error and the index is left
/// untouched. A missing index is a silent no-op, as with `sync_path`.
fn sync_rename(from: &Path, to: &Path) -> Result<(), String> {
    match IndexUpdates::synchronize_rename(&OwnedIndexUpdates::system(), from, to) {
        Ok(SyncOutcome::Matched | SyncOutcome::NoIndex) => Ok(()),
        Ok(SyncOutcome::Unmatched) => Err(format!(
            "{} and {} are not under the same watched folder.",
            from.display(),
            to.display()
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// The thumbnail for one file, mirroring `SearchEngine::thumbnail_path`:
/// an image is its own thumbnail, anything else goes through the shared
/// generator. The engine resolves a result id first, which also proves the
/// file is one it knows; `ts` takes a raw path, so an image that is not
/// there falls through to generation and fails honestly instead of
/// echoing back a path to nothing. The generator keys its cache on the
/// path it is given, so the path is normalized first: two same-named
/// files requested by relative path from different directories must not
/// share one cache entry. Errors still name the path as typed.
fn make_thumbnail(path: &Path) -> Result<PathBuf, String> {
    if is_supported_image(path) && path.is_file() {
        return Ok(owned_index::canonical_spelling(path));
    }
    let video = owned_index::canonical_spelling(path);
    thumbnails::generate(&video)
        .ok_or_else(|| format!("Toka could not make a thumbnail for {}.", path.display()))
}

/// The preview frames for one file, mirroring
/// `SearchEngine::preview_paths` with the same image fast path and the
/// same cache-key normalization as thumbnails.
fn make_preview(path: &Path) -> Result<Vec<PathBuf>, String> {
    if is_supported_image(path) && path.is_file() {
        return Ok(vec![owned_index::canonical_spelling(path)]);
    }
    let video = owned_index::canonical_spelling(path);
    thumbnails::preview(&video)
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
        read_status,
        run_daemon,
        manage_service,
        plan_and_apply_reset,
        &mut stdout,
        &mut stderr,
    );
    if status != 0 {
        std::process::exit(status);
    }
}
