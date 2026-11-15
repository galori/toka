use crate::{
    index_stats::{self, IndexStats},
    reset::{self, ResetPlan},
    search::{MediaType, SearchEngine, SearchFields, SearchRequest, PAGE_SIZE},
};
use std::{
    ffi::{OsStr, OsString},
    io::Write,
};

const SEARCH_USAGE: &str = "Usage: toka search <query>";
const RESET_USAGE: &str = "Usage: toka reset --yes";
const GENERAL_HELP: &str = "Usage: toka <command> [arguments]\n\nCommands:\n  search <query>  Search indexed media and print file paths\n  reset --yes     Remove Toka's search folders, index, caches, and logs\n  --index-stats   Show what the indexer has found and whether it is running";
const RESET_HELP: &str = "Usage: toka reset --yes\n\nPut Toka back to a first-run state. Removes every search folder, the search index and its caches, thumbnail and other caches, and Toka's log files.\nYour media files are not touched. Without --yes, lists what would be removed and changes nothing.";
const INDEX_STATS_HELP: &str = "Usage: toka --index-stats\n\nShow whether the indexer is active, the indexer service state, the number of videos and images indexed, and each search folder's status and file counts. Changes nothing.";
const SEARCH_HELP: &str =
    "Usage: toka search <query>\n\nSearch Toka's indexed videos and images. Print one full path per result line.";

pub fn is_command(arguments: &[OsString]) -> bool {
    arguments.get(1).is_some_and(|argument| {
        argument == OsStr::new("search")
            || argument == OsStr::new("reset")
            || argument == OsStr::new("--index-stats")
            || argument == OsStr::new("--help")
            || argument == OsStr::new("-h")
    })
}

pub fn run_command(
    arguments: &[OsString],
    create_engine: impl FnOnce() -> Result<SearchEngine, String>,
    create_reset_plan: impl FnOnce() -> Result<ResetPlan, String>,
    read_index_stats: impl FnOnce() -> Result<IndexStats, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let Some(command) = arguments.get(1) else {
        let _ = writeln!(stderr, "{GENERAL_HELP}");
        return 2;
    };

    if command == OsStr::new("--help") || command == OsStr::new("-h") {
        return print_help(GENERAL_HELP, stdout, stderr);
    }

    if command == OsStr::new("--index-stats") {
        if arguments.len() == 3
            && (arguments[2] == OsStr::new("--help") || arguments[2] == OsStr::new("-h"))
        {
            return print_help(INDEX_STATS_HELP, stdout, stderr);
        }
        if arguments.len() > 2 {
            let _ = writeln!(stderr, "Usage: toka --index-stats");
            return 2;
        }
        return match read_index_stats() {
            Ok(stats) => {
                let report = index_stats::render(&stats);
                match write!(stdout, "{report}").and_then(|()| stdout.flush()) {
                    Ok(()) => 0,
                    Err(error) => {
                        let _ = writeln!(stderr, "toka --index-stats: could not write: {error}");
                        1
                    }
                }
            }
            Err(error) => {
                let _ = writeln!(stderr, "toka --index-stats: {error}");
                1
            }
        };
    }

    if command == OsStr::new("reset") {
        return run_reset_command(&arguments[2..], create_reset_plan, stdout, stderr);
    }

    if command != OsStr::new("search") {
        let _ = writeln!(stderr, "{GENERAL_HELP}");
        return 2;
    }

    run_search_command(&arguments[2..], create_engine, stdout, stderr)
}

fn run_search_command(
    arguments: &[OsString],
    create_engine: impl FnOnce() -> Result<SearchEngine, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_help(SEARCH_HELP, stdout, stderr);
    }

    let query = match query_from_arguments(arguments) {
        Ok(query) => query,
        Err(message) => {
            let _ = writeln!(stderr, "{message}\n{SEARCH_USAGE}");
            return 2;
        }
    };

    let engine = match create_engine() {
        Ok(engine) => engine,
        Err(error) => {
            let _ = writeln!(stderr, "toka search: {error}");
            return 1;
        }
    };
    run_search(&query, &engine, stdout, stderr)
}

fn run_reset_command(
    arguments: &[OsString],
    create_plan: impl FnOnce() -> Result<ResetPlan, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_help(RESET_HELP, stdout, stderr);
    }
    let confirmed = arguments.len() == 1
        && (arguments[0] == OsStr::new("--yes") || arguments[0] == OsStr::new("-y"));
    if !arguments.is_empty() && !confirmed {
        let _ = writeln!(
            stderr,
            "Reset does not take those arguments.\n{RESET_USAGE}"
        );
        return 2;
    }

    let plan = match create_plan() {
        Ok(plan) => plan,
        Err(error) => {
            let _ = writeln!(stderr, "toka reset: {error}");
            return 1;
        }
    };

    if !confirmed {
        let _ = writeln!(stdout, "Reset would remove these, if present:");
        for path in &plan.paths {
            let _ = writeln!(stdout, "  {}", path.display());
        }
        let _ = writeln!(
            stderr,
            "Nothing was changed. Run `toka reset --yes` to reset."
        );
        return 2;
    }

    let report = reset::apply(&plan);
    for path in &report.removed {
        let _ = writeln!(stdout, "removed {}", path.display());
    }
    for failure in &report.failures {
        let _ = writeln!(stderr, "toka reset: {failure}");
    }
    if !report.failures.is_empty() {
        return 1;
    }
    let _ = writeln!(stdout, "Toka was reset.");
    0
}

fn print_help(help: &str, stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    if let Err(error) = writeln!(stdout, "{help}").and_then(|()| stdout.flush()) {
        let _ = writeln!(stderr, "toka: could not write help: {error}");
        return 1;
    }
    0
}

fn run_search(
    query: &str,
    engine: &SearchEngine,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let mut current_page = 1;
    let mut last_page = None;
    let mut search_id = None;
    let mut paths = Vec::new();

    loop {
        let request = SearchRequest {
            query: query.to_owned(),
            page: current_page,
            page_size: PAGE_SIZE,
            search_id: search_id.clone(),
            fields: SearchFields::default(),
            media_type: MediaType::Both,
        };
        let page = match engine.search(request) {
            Ok(page) => page,
            Err(error) => {
                let _ = writeln!(stderr, "toka search: {error}");
                return 1;
            }
        };
        if search_id.is_none() {
            search_id = Some(page.search_id.clone());
        }
        let last_page = *last_page.get_or_insert(page.total_pages);

        for result in page.results {
            if let Ok(path) = engine.video_path(&result.id) {
                paths.push(path);
            }
        }

        if current_page >= last_page {
            break;
        }
        current_page += 1;
    }

    paths.sort();
    paths.dedup();
    for path in paths {
        if let Err(error) = writeln!(stdout, "{}", path.display()) {
            let _ = writeln!(stderr, "toka search: could not write a result: {error}");
            return 1;
        }
    }
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "toka search: could not write results: {error}");
        return 1;
    }
    0
}

fn query_from_arguments(arguments: &[OsString]) -> Result<String, &'static str> {
    if arguments.is_empty() {
        return Err("Search needs a query.");
    }
    let words = arguments
        .iter()
        .map(|argument| argument.to_str().ok_or("Search terms must be valid text."))
        .collect::<Result<Vec<_>, _>>()?;
    let query = words.join(" ");
    if query.trim().is_empty() {
        return Err("Search needs a query.");
    }
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::run_command;
    use crate::{
        index_stats::{FolderState, FolderStats, IndexStats},
        reset::ResetPlan,
        search::{SearchEngine, SearchError, SearchProvider},
        search_log::SearchLogContext,
    };
    use std::{ffi::OsString, fs, path::PathBuf, sync::Arc};
    use tempfile::tempdir;

    fn no_reset() -> Result<ResetPlan, String> {
        panic!("only reset builds a reset plan")
    }

    fn no_stats() -> Result<IndexStats, String> {
        panic!("only --index-stats reads index stats")
    }

    fn no_engine() -> Result<SearchEngine, String> {
        panic!("reset must not create the search engine")
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

    #[test]
    fn search_command_prints_full_matching_paths() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("family-video.mp4");
        fs::write(&video, b"video").unwrap();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(vec![video.clone()])));
        let args = ["toka", "search", "family video"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_reset,
            no_stats,
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
    fn search_help_does_not_create_the_search_engine() {
        let args = ["toka", "search", "--help"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || panic!("help must not create the search engine"),
            no_reset,
            no_stats,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert!(String::from_utf8(stdout)
            .unwrap()
            .contains("Usage: toka search <query>"));
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_engine_setup_error_returns_a_failure_status() {
        let args = ["toka", "search", "family"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Err("search index could not be opened".into()),
            no_reset,
            no_stats,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "toka search: search index could not be opened\n"
        );
    }

    #[test]
    fn reset_without_yes_lists_what_it_would_remove_and_changes_nothing() {
        let directory = tempdir().unwrap();
        let state = directory.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let args = ["toka", "reset"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            || Ok(ResetPlan::new(vec![state.clone()])),
            no_stats,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(String::from_utf8(stdout)
            .unwrap()
            .contains(&state.display().to_string()));
        assert!(String::from_utf8(stderr).unwrap().contains("--yes"));
        assert!(state.exists());
    }

    #[test]
    fn reset_with_yes_removes_the_planned_paths() {
        let directory = tempdir().unwrap();
        let state = directory.path().join("state");
        fs::create_dir_all(&state).unwrap();
        let args = ["toka", "reset", "--yes"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            || Ok(ResetPlan::new(vec![state.clone()])),
            no_stats,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert!(!state.exists());
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains(&format!("removed {}", state.display())));
        assert!(stderr.is_empty());
    }

    #[test]
    fn reset_help_and_bad_arguments_never_build_a_plan() {
        for (extra, status) in [("--help", 0), ("--force", 2)] {
            let args = ["toka", "reset", extra].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let code = run_command(
                &args,
                no_engine,
                no_reset,
                no_stats,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(code, status);
        }
    }

    #[test]
    fn index_stats_prints_the_report() {
        let args = vec![OsString::from("toka"), OsString::from("--index-stats")];
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let stats = || {
            Ok(IndexStats {
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
            })
        };
        let code = run_command(&args, no_engine, no_reset, stats, &mut stdout, &mut stderr);
        assert_eq!(code, 0);
        let text = String::from_utf8(stdout).unwrap();
        assert!(
            text.contains("Videos:   4") && text.contains("/media"),
            "{text}"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn index_stats_failure_and_extra_arguments_are_errors() {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let args = vec![OsString::from("toka"), OsString::from("--index-stats")];
        let code = run_command(
            &args,
            no_engine,
            no_reset,
            || Err("no index".to_owned()),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 1);
        assert!(String::from_utf8(stderr).unwrap().contains("no index"));

        let mut stderr = Vec::new();
        let args = vec![
            OsString::from("toka"),
            OsString::from("--index-stats"),
            OsString::from("extra"),
        ];
        let code = run_command(
            &args,
            no_engine,
            no_reset,
            no_stats,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 2);
    }
}
