use serde::Serialize;
use std::{
    ffi::{OsStr, OsString},
    io::Write,
    path::PathBuf,
};
use toka_search::{
    index_stats::{self, IndexStats},
    owned_index::{OwnedFolderStatus, OwnedIndexFolder},
    search::{MediaType, SearchEngine, SearchFields, SearchRequest, VideoResult, PAGE_SIZE},
};

/// The version of every `--json` document this CLI prints. The app checks it
/// before parsing.
const JSON_SCHEMA_VERSION: u32 = 1;

const GENERAL_HELP: &str = "Usage: ts <command> [arguments]\n\nCommands:\n  search <query>   Search indexed media and print file paths\n  stats            Show what the indexer has found\n  folders list     List the folders the index watches\n\nOptions:\n  -h, --help      Show this help\n  -v, --version   Show the ts version and Git SHA";
const SEARCH_USAGE: &str = "Usage: ts search <query> [--json] [--limit N] [--page N] [--type video|image|all] [--count] [--verbose]";

const STATS_USAGE: &str = "Usage: ts stats [--json]";
const STATS_HELP: &str = "Usage: ts stats [--json]\n\nShow whether the indexer is active, the number of videos and images indexed, and each search folder's status and file counts. Changes nothing.";
const FOLDERS_HELP: &str = "Usage: ts folders <command>\n\nCommands:\n  list [--json]   List the folders the index watches";
const FOLDERS_LIST_USAGE: &str = "Usage: ts folders list [--json]";
const FOLDERS_LIST_HELP: &str = "Usage: ts folders list [--json]\n\nList the folders the index watches, one per line with its indexing state. Changes nothing.";

fn search_help() -> String {
    format!(
        "{SEARCH_USAGE}\n\nSearch Toka's indexed videos and images. Print one full path per result line.\n\n  --json       Print one JSON document (schema 1) instead of paths\n  --limit N    Print at most N results\n  --page N     Print page N of the results ({PAGE_SIZE} per page)\n  --type T     Only video, image, or all results (default all)\n  --count      Print only how many results matched\n  --verbose    Stream the per-search trace to stderr\n\nUse -- to search for text starting with a dash."
    )
}

fn build_version() -> &'static str {
    pick_version(option_env!("TS_BUILD_VERSION"))
}

fn pick_version(build_version: Option<&'static str>) -> &'static str {
    build_version
        .filter(|version| !version.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// The version line also shows the same Git SHA as the Toka launch screen.
/// The build hands it over as `TOKA_GIT_SHA`; a build with no provenance has
/// none, so it reads "development" exactly as the frontend does.
fn version_text(version: &str, git_sha: Option<&str>) -> String {
    let sha = git_sha
        .filter(|sha| !sha.is_empty())
        .unwrap_or("development");
    format!("ts {version} (Git SHA {sha})")
}

pub fn run_command(
    arguments: &[OsString],
    create_engine: impl FnOnce() -> Result<SearchEngine, String>,
    create_verbose_engine: impl FnOnce() -> Result<SearchEngine, String>,
    read_index_stats: impl FnOnce() -> Result<IndexStats, String>,
    read_folders: impl FnOnce() -> Result<Vec<OwnedIndexFolder>, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let Some(command) = arguments.get(1) else {
        let _ = writeln!(stderr, "{GENERAL_HELP}");
        return 2;
    };

    if command == OsStr::new("--version") || command == OsStr::new("-v") {
        return print_text(
            &version_text(build_version(), option_env!("TOKA_GIT_SHA")),
            stdout,
            stderr,
        );
    }

    if command == OsStr::new("--help") || command == OsStr::new("-h") {
        return print_text(GENERAL_HELP, stdout, stderr);
    }

    if command == OsStr::new("search") {
        return run_search_command(
            &arguments[2..],
            create_engine,
            create_verbose_engine,
            stdout,
            stderr,
        );
    }

    if command == OsStr::new("stats") {
        return run_stats_command(&arguments[2..], read_index_stats, stdout, stderr);
    }

    if command == OsStr::new("folders") {
        return run_folders_command(&arguments[2..], read_folders, stdout, stderr);
    }

    let _ = writeln!(stderr, "{GENERAL_HELP}");
    2
}

fn run_folders_command(
    arguments: &[OsString],
    read_folders: impl FnOnce() -> Result<Vec<OwnedIndexFolder>, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(FOLDERS_HELP, stdout, stderr);
    }
    let Some((subcommand, rest)) = arguments.split_first() else {
        let _ = writeln!(stderr, "{FOLDERS_HELP}");
        return 2;
    };
    if subcommand != OsStr::new("list") {
        let _ = writeln!(stderr, "{FOLDERS_HELP}");
        return 2;
    }
    run_folders_list_command(rest, read_folders, stdout, stderr)
}

fn run_folders_list_command(
    arguments: &[OsString],
    read_folders: impl FnOnce() -> Result<Vec<OwnedIndexFolder>, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(FOLDERS_LIST_HELP, stdout, stderr);
    }
    let Some(json) = parse_json_flag(arguments) else {
        let _ = writeln!(stderr, "{FOLDERS_LIST_USAGE}");
        return 2;
    };

    let folders = match read_folders() {
        Ok(folders) => folders,
        Err(error) => {
            let _ = writeln!(stderr, "ts folders list: {error}");
            return 1;
        }
    };
    if json {
        let document = serde_json::json!({
            "schema": JSON_SCHEMA_VERSION,
            "folders": folders,
        });
        return print_json(&document, "ts folders list", stdout, stderr);
    }
    for folder in folders {
        let mut line = format!(
            "{} [{}]",
            folder.path.display(),
            folder_status_label(folder.status)
        );
        if let Some(message) = &folder.message {
            line.push_str(&format!(" - {message}"));
        }
        if let Err(error) = writeln!(stdout, "{line}") {
            let _ = writeln!(stderr, "ts folders list: could not write a folder: {error}");
            return 1;
        }
    }
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "ts folders list: could not write folders: {error}");
        return 1;
    }
    0
}

/// The human label for a folder state, kept identical to its JSON value (see
/// the folder status label test).
fn folder_status_label(status: OwnedFolderStatus) -> &'static str {
    match status {
        OwnedFolderStatus::Pending => "pending",
        OwnedFolderStatus::Indexing => "indexing",
        OwnedFolderStatus::Ready => "ready",
        OwnedFolderStatus::Offline => "offline",
        OwnedFolderStatus::Error => "error",
    }
}

/// Nothing or `--json`: the whole argument language of the small read-only
/// commands. `None` is a usage error for the caller to report.
fn parse_json_flag(arguments: &[OsString]) -> Option<bool> {
    if arguments.is_empty() {
        return Some(false);
    }
    if arguments.len() == 1 && arguments[0] == OsStr::new("--json") {
        return Some(true);
    }
    None
}

fn run_stats_command(
    arguments: &[OsString],
    read_index_stats: impl FnOnce() -> Result<IndexStats, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(STATS_HELP, stdout, stderr);
    }
    let Some(json) = parse_json_flag(arguments) else {
        let _ = writeln!(stderr, "{STATS_USAGE}");
        return 2;
    };

    let stats = match read_index_stats() {
        Ok(stats) => stats,
        Err(error) => {
            let _ = writeln!(stderr, "ts stats: {error}");
            return 1;
        }
    };
    if json {
        let document = serde_json::json!({
            "schema": JSON_SCHEMA_VERSION,
            "revision": stats.revision,
            "videos": stats.videos,
            "images": stats.images,
            "serviceRunning": stats.service_running,
            "folders": stats.folders,
        });
        return print_json(&document, "ts stats", stdout, stderr);
    }
    let report = index_stats::render(&stats);
    match write!(stdout, "{report}").and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "ts stats: could not write: {error}");
            1
        }
    }
}

struct SearchOptions {
    query: String,
    limit: Option<usize>,
    page: Option<usize>,
    media_type: MediaType,
    count: bool,
    json: bool,
    verbose: bool,
}

/// A search hit with the full path the engine resolved it to. The rest is
/// the engine's own result shape, so the JSON contract stays one shape.
#[derive(Serialize)]
struct JsonSearchHit {
    path: PathBuf,
    #[serde(flatten)]
    hit: VideoResult,
}

struct SearchSummary {
    total_results: usize,
    total_pages: usize,
    index_revision: u64,
}

fn run_search_command(
    arguments: &[OsString],
    create_engine: impl FnOnce() -> Result<SearchEngine, String>,
    create_verbose_engine: impl FnOnce() -> Result<SearchEngine, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(&search_help(), stdout, stderr);
    }

    let options = match parse_search_arguments(arguments) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(stderr, "{message}\n{SEARCH_USAGE}");
            return 2;
        }
    };

    let engine = match if options.verbose {
        create_verbose_engine()
    } else {
        create_engine()
    } {
        Ok(engine) => engine,
        Err(error) => {
            let _ = writeln!(stderr, "ts search: {error}");
            return 1;
        }
    };
    if options.count {
        return run_search_count(&options, &engine, stdout, stderr);
    }
    run_search(&options, &engine, stdout, stderr)
}

/// Streams the trace the last search recorded to stderr, so `--verbose`
/// shows it while stdout keeps clean results.
fn write_verbose_trace(engine: &SearchEngine, stderr: &mut impl Write) -> std::io::Result<()> {
    let trace = engine.take_verbose_trace();
    write!(stderr, "{trace}").and_then(|()| stderr.flush())
}

fn run_search_count(
    options: &SearchOptions,
    engine: &SearchEngine,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let request = SearchRequest {
        query: options.query.clone(),
        page: 1,
        page_size: PAGE_SIZE,
        search_id: None,
        fields: SearchFields::default(),
        media_type: options.media_type,
    };
    match engine.match_count(request) {
        Ok(count) => {
            if options.verbose {
                if let Err(error) = write_verbose_trace(engine, stderr) {
                    let _ = writeln!(
                        stderr,
                        "ts search: could not write the search trace: {error}"
                    );
                    return 1;
                }
            }
            if options.json {
                let document = serde_json::json!({
                    "schema": JSON_SCHEMA_VERSION,
                    "query": options.query,
                    "totalResults": count,
                });
                return print_json(&document, "ts search", stdout, stderr);
            }
            if let Err(error) = writeln!(stdout, "{count}").and_then(|()| stdout.flush()) {
                let _ = writeln!(stderr, "ts search: could not write the count: {error}");
                return 1;
            }
            0
        }
        Err(error) => {
            if options.verbose {
                let _ = write_verbose_trace(engine, stderr);
            }
            let _ = writeln!(stderr, "ts search: {error}");
            1
        }
    }
}

fn print_json(
    document: &serde_json::Value,
    command: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    match serde_json::to_string_pretty(document) {
        Ok(text) => {
            if let Err(error) = writeln!(stdout, "{text}").and_then(|()| stdout.flush()) {
                let _ = writeln!(stderr, "{command}: could not write results: {error}");
                return 1;
            }
            0
        }
        Err(error) => {
            let _ = writeln!(stderr, "{command}: could not encode results: {error}");
            1
        }
    }
}

fn run_search(
    options: &SearchOptions,
    engine: &SearchEngine,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    // The engine only serves a page from an earlier one, so a requested page
    // is walked to from the first; pages past the last are empty, so asking
    // beyond the end stops at the end.
    let mut current_page = 1;
    let mut summary = None;
    let mut search_id = None;
    let mut hits = Vec::new();

    loop {
        let request = SearchRequest {
            query: options.query.clone(),
            page: current_page,
            page_size: PAGE_SIZE,
            search_id: search_id.clone(),
            fields: SearchFields::default(),
            media_type: options.media_type,
        };
        let page = match engine.search(request) {
            Ok(page) => page,
            Err(error) => {
                if options.verbose {
                    let _ = write_verbose_trace(engine, stderr);
                }
                let _ = writeln!(stderr, "ts search: {error}");
                return 1;
            }
        };
        if options.verbose {
            if let Err(error) = write_verbose_trace(engine, stderr) {
                let _ = writeln!(
                    stderr,
                    "ts search: could not write the search trace: {error}"
                );
                return 1;
            }
        }
        if search_id.is_none() {
            search_id = Some(page.search_id.clone());
        }
        // Every page of one walk shares a snapshot, so the first page's
        // totals describe them all.
        let summary = summary.get_or_insert(SearchSummary {
            total_results: page.total_results,
            total_pages: page.total_pages,
            index_revision: page.index_revision,
        });

        if options.page.is_none_or(|wanted| wanted == current_page) {
            for result in page.results {
                if let Ok(path) = engine.video_path(&result.id) {
                    hits.push(JsonSearchHit { path, hit: result });
                }
            }
        }

        if options.page.is_some_and(|wanted| current_page >= wanted)
            || current_page >= summary.total_pages
        {
            break;
        }
        current_page += 1;
    }

    hits.sort_by(|left, right| left.path.cmp(&right.path));
    hits.dedup_by(|left, right| left.path == right.path);
    if let Some(limit) = options.limit {
        hits.truncate(limit);
    }
    if options.json {
        // The totals describe the whole search; the page fields describe
        // this document, which holds one requested page or every hit.
        let summary = summary.expect("a search walk fetches at least one page");
        let (page, page_size, total_pages) = match options.page {
            Some(wanted) => (wanted, PAGE_SIZE, summary.total_pages),
            None => (1, hits.len(), 1),
        };
        let document = serde_json::json!({
            "schema": JSON_SCHEMA_VERSION,
            "query": options.query,
            "page": page,
            "pageSize": page_size,
            "totalResults": summary.total_results,
            "totalPages": total_pages,
            "indexRevision": summary.index_revision,
            "results": hits,
        });
        return print_json(&document, "ts search", stdout, stderr);
    }
    for hit in hits {
        if let Err(error) = writeln!(stdout, "{}", hit.path.display()) {
            let _ = writeln!(stderr, "ts search: could not write a result: {error}");
            return 1;
        }
    }
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "ts search: could not write results: {error}");
        return 1;
    }
    0
}

fn parse_search_arguments(arguments: &[OsString]) -> Result<SearchOptions, String> {
    let mut options = SearchOptions {
        query: String::new(),
        limit: None,
        page: None,
        media_type: MediaType::Both,
        count: false,
        json: false,
        verbose: false,
    };
    let mut words: Vec<&str> = Vec::new();
    let mut only_words = false;
    let mut index = 0;
    while index < arguments.len() {
        let Some(text) = arguments[index].to_str() else {
            return Err("Search terms must be valid text.".to_owned());
        };
        if only_words {
            words.push(text);
        } else if text == "--" {
            only_words = true;
        } else if text == "--limit" {
            options.limit = Some(take_search_number("--limit", arguments, &mut index)?);
        } else if text == "--page" {
            options.page = Some(take_search_number("--page", arguments, &mut index)?);
        } else if text == "--count" {
            options.count = true;
        } else if text == "--json" {
            options.json = true;
        } else if text == "--verbose" {
            options.verbose = true;
        } else if text == "--type" {
            index += 1;
            let value = arguments
                .get(index)
                .and_then(|value| value.to_str())
                .ok_or_else(|| "--type needs one of: video, image, all.".to_owned())?;
            options.media_type = match value {
                "video" => MediaType::Videos,
                "image" => MediaType::Images,
                "all" => MediaType::Both,
                _ => {
                    return Err(format!(
                        "Unknown type '{value}'. Use one of: video, image, all."
                    ));
                }
            };
        } else if text.starts_with('-') {
            return Err(format!("Unknown option '{text}'."));
        } else {
            words.push(text);
        }
        index += 1;
    }
    let query = words.join(" ");
    if query.trim().is_empty() {
        return Err("Search needs a query.".to_owned());
    }
    if options.count && (options.page.is_some() || options.limit.is_some()) {
        return Err("--count cannot be combined with --page or --limit.".to_owned());
    }
    options.query = query;
    Ok(options)
}

fn take_search_number(
    flag: &str,
    arguments: &[OsString],
    index: &mut usize,
) -> Result<usize, String> {
    *index += 1;
    let value = arguments
        .get(*index)
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("{flag} needs a positive number."))?;
    value
        .parse::<usize>()
        .ok()
        .filter(|number| *number >= 1)
        .ok_or_else(|| format!("{flag} needs a positive number, got '{value}'."))
}

fn print_text(text: &str, stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    if let Err(error) = writeln!(stdout, "{text}").and_then(|()| stdout.flush()) {
        let _ = writeln!(stderr, "ts: could not write output: {error}");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::run_command;
    use std::{ffi::OsString, fs, path::PathBuf, sync::Arc};
    use tempfile::tempdir;
    use toka_search::{
        index_stats::{FolderState, FolderStats, IndexStats},
        owned_index::{OwnedFolderStatus, OwnedIndexFolder},
        search::{SearchEngine, SearchError, SearchProvider},
        search_log::SearchLogContext,
    };

    fn no_engine() -> Result<SearchEngine, String> {
        panic!("this command must not create the search engine")
    }

    fn no_verbose_engine() -> Result<SearchEngine, String> {
        panic!("this command must not create the verbose search engine")
    }

    fn no_stats() -> Result<IndexStats, String> {
        panic!("this command must not read index stats")
    }

    fn no_folders() -> Result<Vec<OwnedIndexFolder>, String> {
        panic!("this command must not read folders")
    }

    #[test]
    fn version_prefers_the_build_version_over_the_manifest_version() {
        assert_eq!(
            super::pick_version(Some("0.1.1+482.a1b2c3d.feat")),
            "0.1.1+482.a1b2c3d.feat"
        );
        assert_eq!(super::pick_version(None), env!("CARGO_PKG_VERSION"));
        assert_eq!(super::pick_version(Some("")), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn version_text_includes_the_git_sha_or_development() {
        assert_eq!(
            super::version_text("0.1.1+5.abc", Some("abc123")),
            "ts 0.1.1+5.abc (Git SHA abc123)"
        );
        assert_eq!(
            super::version_text("0.1.1", None),
            "ts 0.1.1 (Git SHA development)"
        );
        assert_eq!(
            super::version_text("0.1.1", Some("")),
            super::version_text("0.1.1", None)
        );
    }

    #[test]
    fn help_flags_print_without_building_anything() {
        for flag in ["-h", "--help"] {
            let args = ["ts", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            let output = String::from_utf8(stdout).unwrap();
            assert!(output.contains("Usage: ts <command>"), "{output}");
            for command in ["search", "stats", "folders", "--version"] {
                assert!(output.contains(command), "{output}");
            }
            assert!(stderr.is_empty());
        }
    }

    struct FixtureProvider(Vec<PathBuf>);

    impl SearchProvider for FixtureProvider {
        fn candidates(
            &self,
            _query: &str,
            _whole_path: bool,
            _context: &SearchLogContext,
        ) -> Result<Vec<PathBuf>, SearchError> {
            Ok(self.0.clone())
        }
    }

    struct ErrorProvider;

    impl SearchProvider for ErrorProvider {
        fn candidates(
            &self,
            _query: &str,
            _whole_path: bool,
            _context: &SearchLogContext,
        ) -> Result<Vec<PathBuf>, SearchError> {
            Err(SearchError::Provider("backend unavailable".into()))
        }
    }

    fn verbose_engine(provider: impl SearchProvider + 'static) -> SearchEngine {
        SearchEngine::new_with_logger(
            Arc::new(provider),
            Arc::new(toka_search::search_log::SearchLogger::with_verbose_capture()),
        )
    }

    #[test]
    fn search_command_prints_full_matching_paths() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("family-video.mp4");
        fs::write(&video, b"video").unwrap();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(vec![video.clone()])));
        let args = ["ts", "search", "family video"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!("{}\n", video.display())
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_without_a_query_prints_usage() {
        let args = ["ts", "search"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("Search needs a query.\n{}\n", super::SEARCH_USAGE)
        );
    }

    #[test]
    fn search_help_does_not_create_the_search_engine() {
        for flag in ["--help", "-h"] {
            let args = ["ts", "search", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                || panic!("help must not create the search engine"),
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            assert!(String::from_utf8(stdout)
                .unwrap()
                .contains(super::SEARCH_USAGE));
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn search_engine_setup_error_returns_a_failure_status() {
        let args = ["ts", "search", "family"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Err("search index could not be opened".into()),
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts search: search index could not be opened\n"
        );
    }

    #[test]
    fn search_type_filters_to_videos_or_images() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("family-video.mp4");
        let image = directory.path().join("family-image.jpg");
        fs::write(&video, b"video").unwrap();
        fs::write(&image, b"image").unwrap();
        for (flag, expected) in [
            ("video", vec![video.clone()]),
            ("image", vec![image.clone()]),
            ("all", vec![image.clone(), video.clone()]),
        ] {
            let engine = SearchEngine::new(Arc::new(FixtureProvider(vec![
                video.clone(),
                image.clone(),
            ])));
            let args = ["ts", "search", "family", "--type", flag]
                .map(OsString::from)
                .to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                || Ok(engine),
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            let expected = expected
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            assert_eq!(String::from_utf8(stdout).unwrap(), expected, "{flag}");
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn search_limit_caps_the_printed_results() {
        let directory = tempdir().unwrap();
        let mut videos = (1..=3)
            .map(|number| {
                let path = directory.path().join(format!("family-0{number}.mp4"));
                fs::write(&path, b"video").unwrap();
                path
            })
            .collect::<Vec<_>>();
        videos.sort();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos.clone())));
        let args = ["ts", "search", "family", "--limit", "2"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let expected = videos
            .iter()
            .take(2)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert_eq!(String::from_utf8(stdout).unwrap(), expected);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_page_prints_only_that_page() {
        let directory = tempdir().unwrap();
        let videos = (0..25)
            .map(|number| {
                let path = directory
                    .path()
                    .join(format!("family-trip-{number:02}.mp4"));
                fs::write(&path, b"video").unwrap();
                path
            })
            .collect::<Vec<_>>();
        for (page, expected_lines) in [("1", 24), ("2", 1)] {
            let engine = SearchEngine::new(Arc::new(FixtureProvider(videos.clone())));
            let args = ["ts", "search", "family", "--page", page]
                .map(OsString::from)
                .to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                || Ok(engine),
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "page {page}");
            let output = String::from_utf8(stdout).unwrap();
            let lines = output.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), expected_lines, "page {page}");
            for line in lines {
                assert!(
                    videos
                        .iter()
                        .any(|video| video.display().to_string() == line),
                    "unexpected path: {line}"
                );
            }
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn search_count_prints_only_the_match_count() {
        let directory = tempdir().unwrap();
        let videos = ["family-one.mp4", "family-two.mp4", "unrelated.mp4"]
            .map(|name| {
                let path = directory.path().join(name);
                fs::write(&path, b"video").unwrap();
                path
            })
            .to_vec();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos)));
        let args = ["ts", "search", "family", "--count"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(String::from_utf8(stdout).unwrap(), "2\n");
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_count_with_page_or_limit_is_a_usage_error() {
        for arguments in [
            ["ts", "search", "family", "--count", "--page", "1"],
            ["ts", "search", "family", "--count", "--limit", "1"],
        ] {
            let args = arguments.map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            let error = String::from_utf8(stderr).unwrap();
            assert!(error.contains("--count cannot be combined"), "{error}");
            assert!(error.contains(super::SEARCH_USAGE), "{error}");
        }
    }

    #[test]
    fn search_json_prints_a_versioned_document() {
        let directory = tempdir().unwrap();
        let mut videos = ["family-one.mp4", "family-two.mp4"]
            .map(|name| {
                let path = directory.path().join(name);
                fs::write(&path, b"video").unwrap();
                path
            })
            .to_vec();
        videos.sort();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos.clone())));
        let args = ["ts", "search", "family", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["query"], "family");
        assert_eq!(document["page"], 1);
        assert_eq!(document["pageSize"], 2);
        assert_eq!(document["totalResults"], 2);
        assert_eq!(document["totalPages"], 1);
        assert!(document["indexRevision"].is_number());
        let results = document["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        for (result, video) in results.iter().zip(videos.iter()) {
            assert_eq!(result["path"], video.display().to_string());
            assert!(!result["id"].as_str().unwrap().is_empty());
            assert_eq!(result["extension"], "mp4");
            assert!(result["tags"].is_array());
            assert!(result["folderTags"].is_array());
        }
        assert_eq!(results[0]["fileName"], "family-one.mp4");
        assert_eq!(results[1]["fileName"], "family-two.mp4");
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_json_with_page_reports_that_page() {
        let directory = tempdir().unwrap();
        let videos = (0..25)
            .map(|number| {
                let path = directory
                    .path()
                    .join(format!("family-trip-{number:02}.mp4"));
                fs::write(&path, b"video").unwrap();
                path
            })
            .collect::<Vec<_>>();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos.clone())));
        let args = ["ts", "search", "family", "--page", "2", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["page"], 2);
        assert_eq!(document["pageSize"], toka_search::search::PAGE_SIZE as u64);
        assert_eq!(document["totalResults"], 25);
        assert_eq!(document["totalPages"], 2);
        let results = document["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert!(videos
            .iter()
            .any(|video| results[0]["path"] == video.display().to_string()));
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_json_with_no_matches_is_an_empty_document() {
        let engine = SearchEngine::new(Arc::new(FixtureProvider(Vec::new())));
        let args = ["ts", "search", "missing", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["totalResults"], 0);
        assert_eq!(document["results"].as_array().unwrap().len(), 0);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_json_count_prints_total_results() {
        let directory = tempdir().unwrap();
        let videos = ["family-one.mp4", "family-two.mp4", "unrelated.mp4"]
            .map(|name| {
                let path = directory.path().join(name);
                fs::write(&path, b"video").unwrap();
                path
            })
            .to_vec();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos)));
        let args = ["ts", "search", "family", "--count", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["query"], "family");
        assert_eq!(document["totalResults"], 2);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_verbose_streams_the_trace_to_stderr() {
        let directory = tempdir().unwrap();
        let matched = directory.path().join("summer-trip.mp4");
        fs::write(&matched, b"video").unwrap();
        let skipped = directory.path().join("winter-trip.mp4");
        let engine = verbose_engine(FixtureProvider(vec![matched.clone(), skipped.clone()]));
        let args = ["ts", "search", "summer", "--verbose"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || panic!("verbose search must not create the plain engine"),
            || Ok(engine),
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!("{}\n", matched.display())
        );
        let trace = String::from_utf8(stderr).unwrap();
        assert!(
            trace.contains(&format!(
                "FILTERED OUT: {} (query did not match)",
                skipped.display()
            )),
            "{trace}"
        );
        assert!(trace.contains("RESULTS RETURNED TO APP:"), "{trace}");
        assert!(trace.contains(&matched.display().to_string()), "{trace}");
    }

    #[test]
    fn search_verbose_count_streams_filtered_paths() {
        let directory = tempdir().unwrap();
        let matched = directory.path().join("summer-trip.mp4");
        fs::write(&matched, b"video").unwrap();
        let skipped = directory.path().join("winter-trip.mp4");
        let engine = verbose_engine(FixtureProvider(vec![matched, skipped.clone()]));
        let args = ["ts", "search", "summer", "--count", "--verbose"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || panic!("verbose search must not create the plain engine"),
            || Ok(engine),
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(String::from_utf8(stdout).unwrap(), "1\n");
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains(&format!("FILTERED OUT: {}", skipped.display())));
    }

    #[test]
    fn search_verbose_reports_errors_with_the_trace() {
        let engine = verbose_engine(ErrorProvider);
        let args = ["ts", "search", "family", "--verbose"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || panic!("verbose search must not create the plain engine"),
            || Ok(engine),
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        let error = String::from_utf8(stderr).unwrap();
        assert!(
            error.contains("SEARCH ERROR: backend unavailable"),
            "{error}"
        );
        assert!(error.contains("ts search: backend unavailable"), "{error}");
    }

    #[test]
    fn search_provider_error_returns_a_failure_status() {
        let engine = SearchEngine::new(Arc::new(ErrorProvider));
        let args = ["ts", "search", "family"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts search: backend unavailable\n"
        );
    }

    #[test]
    fn search_with_an_unknown_type_is_a_usage_error() {
        let args = ["ts", "search", "family", "--type", "audio"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        let error = String::from_utf8(stderr).unwrap();
        assert!(error.contains("Unknown type 'audio'"), "{error}");
        assert!(error.contains(super::SEARCH_USAGE), "{error}");
    }

    #[test]
    fn search_with_an_unknown_option_is_a_usage_error() {
        let args = ["ts", "search", "family", "--bogus"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        let error = String::from_utf8(stderr).unwrap();
        assert!(error.contains("Unknown option '--bogus'"), "{error}");
        assert!(error.contains(super::SEARCH_USAGE), "{error}");
    }

    #[test]
    fn search_numbers_must_be_positive() {
        for arguments in [
            vec!["ts", "search", "family", "--limit"],
            vec!["ts", "search", "family", "--limit", "zero"],
            vec!["ts", "search", "family", "--limit", "0"],
            vec!["ts", "search", "family", "--page"],
            vec!["ts", "search", "family", "--page", "1.5"],
            vec!["ts", "search", "family", "--page", "0"],
            vec!["ts", "search", "family", "--type"],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            assert!(
                String::from_utf8(stderr)
                    .unwrap()
                    .contains(super::SEARCH_USAGE),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn search_with_only_flags_still_needs_a_query() {
        let args = ["ts", "search", "--type", "video"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("Search needs a query.\n{}\n", super::SEARCH_USAGE)
        );
    }

    #[test]
    fn search_after_a_dash_separator_takes_flags_literally() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("--limit-fan.mp4");
        fs::write(&video, b"video").unwrap();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(vec![video.clone()])));
        let args = ["ts", "search", "--", "--limit-fan"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!("{}\n", video.display())
        );
        assert!(stderr.is_empty());
    }

    fn sample_stats() -> IndexStats {
        IndexStats {
            revision: 1,
            videos: 4,
            images: 1,
            service_running: Some(true),
            folders: vec![FolderStats {
                path: "/media".into(),
                state: FolderState::Ready,
                message: None,
                scanned_files: 0,
                videos: Some(4),
                images: Some(1),
            }],
        }
    }

    #[test]
    fn stats_prints_the_report() {
        let args = ["ts", "stats"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            || Ok(sample_stats()),
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let text = String::from_utf8(stdout).unwrap();
        assert!(
            text.contains("Videos:   4") && text.contains("/media"),
            "{text}"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn stats_json_prints_a_versioned_document() {
        let args = ["ts", "stats", "--json"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            || Ok(sample_stats()),
            no_folders,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["revision"], 1);
        assert_eq!(document["videos"], 4);
        assert_eq!(document["images"], 1);
        assert_eq!(document["serviceRunning"], true);
        assert_eq!(document["folders"][0]["path"], "/media");
        assert_eq!(document["folders"][0]["state"], "ready");
        assert!(stderr.is_empty());
    }

    #[test]
    fn stats_help_does_not_read_stats() {
        for flag in ["--help", "-h"] {
            let args = ["ts", "stats", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            assert!(String::from_utf8(stdout)
                .unwrap()
                .contains(super::STATS_USAGE));
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn stats_failure_and_extra_arguments_are_errors() {
        let args = ["ts", "stats"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            || Err("no index".to_owned()),
            no_folders,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, 1);
        assert_eq!(String::from_utf8(stderr).unwrap(), "ts stats: no index\n");

        let args = ["ts", "stats", "extra"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("{}\n", super::STATS_USAGE)
        );
    }

    fn sample_folders() -> Vec<OwnedIndexFolder> {
        vec![
            OwnedIndexFolder {
                id: "abc".into(),
                path: "/media/movies".into(),
                status: OwnedFolderStatus::Ready,
                message: None,
                scanned_files: 0,
                current_path: None,
                recent_directories: Vec::new(),
            },
            OwnedIndexFolder {
                id: "def".into(),
                path: "/media/gone".into(),
                status: OwnedFolderStatus::Offline,
                message: Some("not mounted".into()),
                scanned_files: 0,
                current_path: None,
                recent_directories: Vec::new(),
            },
        ]
    }

    #[test]
    fn folders_list_prints_one_folder_per_line() {
        let args = ["ts", "folders", "list"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(sample_folders()),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "/media/movies [ready]\n/media/gone [offline] - not mounted\n"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_list_with_no_folders_prints_nothing() {
        let args = ["ts", "folders", "list"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(Vec::new()),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn folder_status_labels_match_the_json_contract() {
        for (status, label) in [
            (OwnedFolderStatus::Pending, "pending"),
            (OwnedFolderStatus::Indexing, "indexing"),
            (OwnedFolderStatus::Ready, "ready"),
            (OwnedFolderStatus::Offline, "offline"),
            (OwnedFolderStatus::Error, "error"),
        ] {
            assert_eq!(super::folder_status_label(status), label);
            assert_eq!(serde_json::to_value(status).unwrap(), label);
        }
    }

    #[test]
    fn folders_list_json_prints_a_versioned_document() {
        let args = ["ts", "folders", "list", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(sample_folders()),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        let folders = document["folders"].as_array().unwrap();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0]["id"], "abc");
        assert_eq!(folders[0]["path"], "/media/movies");
        assert_eq!(folders[0]["status"], "ready");
        assert_eq!(folders[1]["status"], "offline");
        assert_eq!(folders[1]["message"], "not mounted");
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_help_does_not_read_folders() {
        for (arguments, expected) in [
            (
                vec!["ts", "folders", "--help"],
                "Usage: ts folders <command>",
            ),
            (vec!["ts", "folders", "-h"], "Usage: ts folders <command>"),
            (
                vec!["ts", "folders", "list", "--help"],
                super::FOLDERS_LIST_USAGE,
            ),
            (
                vec!["ts", "folders", "list", "-h"],
                super::FOLDERS_LIST_USAGE,
            ),
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{arguments:?}");
            assert!(
                String::from_utf8(stdout).unwrap().contains(expected),
                "{arguments:?}"
            );
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn folders_without_list_is_a_usage_error() {
        for arguments in [
            vec!["ts", "folders"],
            vec!["ts", "folders", "add", "/media"],
            vec!["ts", "folders", "list", "extra"],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            assert!(!stderr.is_empty());
        }
    }

    #[test]
    fn folders_list_failure_returns_a_failure_status() {
        let args = ["ts", "folders", "list"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Err("no index".to_owned()),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts folders list: no index\n"
        );
    }

    #[test]
    fn bare_and_unknown_commands_print_help_to_stderr() {
        for args in [
            vec![OsString::from("ts")],
            ["ts", "dance"].map(OsString::from).to_vec(),
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2);
            assert!(stdout.is_empty());
            assert!(String::from_utf8(stderr)
                .unwrap()
                .contains("Usage: ts <command>"));
        }
    }

    #[test]
    fn version_flags_print_without_building_anything() {
        for flag in ["-v", "--version"] {
            let args = ["ts", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            let output = String::from_utf8(stdout).unwrap();
            let prefix = format!("ts {} (Git SHA ", super::build_version());
            let sha = output
                .strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(")\n"))
                .unwrap_or_else(|| panic!("unexpected version line: {output}"));
            assert_eq!(
                Some(sha),
                option_env!("TOKA_GIT_SHA").or(Some("development"))
            );
            assert!(stderr.is_empty());
        }
    }
}
