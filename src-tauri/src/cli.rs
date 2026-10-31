use crate::search::{MediaType, SearchEngine, SearchFields, SearchRequest, PAGE_SIZE};
use std::{
    ffi::{OsStr, OsString},
    io::Write,
};

const SEARCH_USAGE: &str = "Usage: toka search <query>";
const GENERAL_HELP: &str = "Usage: toka <command> [arguments]\n\nCommands:\n  search <query>  Search indexed media and print file paths";
const SEARCH_HELP: &str =
    "Usage: toka search <query>\n\nSearch Toka's indexed videos and images. Print one full path per result line.";

pub fn is_command(arguments: &[OsString]) -> bool {
    arguments.get(1).is_some_and(|argument| {
        argument == OsStr::new("search")
            || argument == OsStr::new("--help")
            || argument == OsStr::new("-h")
    })
}

pub fn run_command(
    arguments: &[OsString],
    create_engine: impl FnOnce() -> Result<SearchEngine, String>,
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
    let mut paths = Vec::new();

    loop {
        let request = SearchRequest {
            query: query.to_owned(),
            page: current_page,
            page_size: PAGE_SIZE,
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
        search::{SearchEngine, SearchError, SearchProvider},
        search_log::SearchLogContext,
    };
    use std::{ffi::OsString, fs, path::PathBuf, sync::Arc};
    use tempfile::tempdir;

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

        let status = run_command(&args, || Ok(engine), &mut stdout, &mut stderr);

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
}
