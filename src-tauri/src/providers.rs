#[cfg(target_os = "linux")]
use crate::managed_index::{self, IndexPaths};
use crate::search::{SearchError, SearchProvider};
use crate::search_log::{SearchLogContext, SearchLogger};
#[cfg(all(target_os = "macos", not(test)))]
use std::sync::Once;
use std::{path::PathBuf, sync::Arc};

#[cfg(any(target_os = "macos", test))]
use std::path::Path;
use std::process::Command;

#[derive(Debug)]
struct ProcessOutput {
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

trait ProcessRunner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        context: &SearchLogContext,
    ) -> Result<ProcessOutput, std::io::Error>;
}

struct SystemProcessRunner {
    logger: Arc<SearchLogger>,
}

impl SystemProcessRunner {
    fn new(logger: Arc<SearchLogger>) -> Self {
        Self { logger }
    }
}

impl ProcessRunner for SystemProcessRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        context: &SearchLogContext,
    ) -> Result<ProcessOutput, std::io::Error> {
        let output = match Command::new(program).args(args).output() {
            Ok(output) => output,
            Err(error) => {
                self.logger
                    .record_error(context, &format!("{program} could not start: {error}"));
                return Err(error);
            }
        };
        let process_output = ProcessOutput {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        self.logger.record_command(
            context,
            program,
            args,
            process_output.success,
            process_output.exit_code,
            &process_output.stderr,
        );
        Ok(process_output)
    }
}

#[cfg(any(target_os = "macos", test))]
pub struct MdfindSearchProvider {
    runner: Arc<dyn ProcessRunner>,
}

#[cfg(any(target_os = "macos", test))]
impl MdfindSearchProvider {
    #[cfg(target_os = "macos")]
    pub fn system(logger: Arc<SearchLogger>) -> Self {
        Self {
            runner: Arc::new(SystemProcessRunner::new(logger)),
        }
    }
}

#[cfg(any(target_os = "macos", test))]
impl SearchProvider for MdfindSearchProvider {
    fn candidates(
        &self,
        query: &str,
        whole_path: bool,
        context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError> {
        prepare_macos_downloads_folder_access();
        let term = longest_term(query)?;
        let escaped = term.replace('\\', "\\\\").replace('"', "\\\"");
        // Spotlight's display name is the file's own name; its path attribute
        // covers the folders above it, which is what a folder search needs.
        let attribute = if whole_path {
            "kMDItemPath"
        } else {
            "kMDItemDisplayName"
        };
        let predicate = format!(
            "{attribute} == \"*{escaped}*\"cd && kMDItemContentTypeTree == \"public.movie\""
        );
        let mut paths = run_mdfind(&*self.runner, vec![predicate], context)?;
        paths.extend(macos_download_candidates(query)?);
        Ok(paths)
    }
}

#[cfg(any(target_os = "macos", test))]
fn run_mdfind(
    runner: &dyn ProcessRunner,
    args: Vec<String>,
    context: &SearchLogContext,
) -> Result<Vec<PathBuf>, SearchError> {
    let output = runner
        .run("/usr/bin/mdfind", &args, context)
        .map_err(|error| {
            SearchError::Provider(format!(
            "Spotlight search could not start. Check macOS privacy and indexing settings: {error}"
        ))
        })?;
    parse_output(output, "Spotlight search failed")
}

#[cfg(all(target_os = "macos", not(test)))]
fn macos_download_candidates(query: &str) -> Result<Vec<PathBuf>, SearchError> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(Vec::new());
    };
    Ok(download_folder_candidates(
        &PathBuf::from(home).join("Downloads"),
        query,
    ))
}

#[cfg(test)]
fn macos_download_candidates(_query: &str) -> Result<Vec<PathBuf>, SearchError> {
    Ok(Vec::new())
}

#[cfg(any(target_os = "macos", test))]
fn download_folder_candidates(root: &Path, query: &str) -> Vec<PathBuf> {
    let Ok(term) = longest_term(query) else {
        return Vec::new();
    };
    let term = term.to_lowercase();
    let mut candidates = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
            } else if path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
                .contains(&term)
            {
                candidates.push(path);
            }
        }
    }

    candidates
}

#[cfg(all(target_os = "macos", not(test)))]
fn prepare_macos_downloads_folder_access() {
    static DOWNLOADS_ACCESS: Once = Once::new();
    DOWNLOADS_ACCESS.call_once(|| {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let _ = std::fs::read_dir(PathBuf::from(home).join("Downloads"));
    });
}

#[cfg(test)]
fn prepare_macos_downloads_folder_access() {}

#[cfg(any(target_os = "linux", test))]
pub struct RecollSearchProvider {
    runner: Arc<dyn ProcessRunner>,
}

#[cfg(any(target_os = "linux", test))]
impl RecollSearchProvider {
    #[cfg(target_os = "linux")]
    pub fn system(logger: Arc<SearchLogger>) -> Self {
        Self {
            runner: Arc::new(SystemProcessRunner::new(logger)),
        }
    }
}

#[cfg(any(target_os = "linux", test))]
impl SearchProvider for RecollSearchProvider {
    // Recoll indexes names and contents, and its query language has no
    // substring match over a file's folders, so a folder search here can only
    // be answered from the names Recoll already returns. Every other provider
    // widens; this one is the reason a path search is worth having plocate for.
    fn candidates(
        &self,
        query: &str,
        _whole_path: bool,
        context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError> {
        let term = longest_term(query)?;
        // Leading wildcard also prevents a query beginning with `-` from being
        // interpreted as another command-line option.
        let filename_query = format!("*{term}*");
        let args = ["-f", "-b", "--paths-only", "-C", "-n", "0", &filename_query]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let output = self.runner.run("recollq", &args, context).map_err(|error| {
            SearchError::Provider(format!(
                "Recoll search could not start. Install Recoll and create an index with recollindex: {error}"
            ))
        })?;
        parse_output(
            output,
            "Recoll search failed. Ensure Recoll is installed and its index has been built",
        )
    }
}

#[cfg(any(target_os = "linux", test))]
pub struct PlocateSearchProvider {
    runner: Arc<dyn ProcessRunner>,
}

#[cfg(any(target_os = "linux", test))]
impl PlocateSearchProvider {
    #[cfg(target_os = "linux")]
    pub fn system(logger: Arc<SearchLogger>) -> Self {
        Self {
            runner: Arc::new(SystemProcessRunner::new(logger)),
        }
    }
}

#[cfg(any(target_os = "linux", test))]
impl SearchProvider for PlocateSearchProvider {
    fn candidates(
        &self,
        query: &str,
        whole_path: bool,
        context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError> {
        let term = longest_term(query)?;
        // plocate matches the whole path unless it is confined to the base
        // name, so a folder search is the flag left off rather than added.
        let mut args = vec!["--ignore-case"];
        if !whole_path {
            args.push("--basename");
        }
        args.extend(["--existing", "--", term]);
        let args = args.into_iter().map(String::from).collect::<Vec<_>>();
        let output = self.runner.run("plocate", &args, context).map_err(|error| SearchError::Provider(format!("plocate search could not start. Install plocate and build its index with updatedb: {error}")))?;
        // plocate uses exit status 1 to report a successful search with no
        // matches. Other non-zero statuses still indicate a provider error.
        if output.exit_code == Some(1) {
            return Ok(Vec::new());
        }
        parse_output(
            output,
            "plocate search failed. Ensure plocate is installed and its index has been built",
        )
    }
}

#[cfg(target_os = "linux")]
pub struct ManagedPlocateSearchProvider {
    runner: Arc<dyn ProcessRunner>,
    paths: IndexPaths,
}

#[cfg(target_os = "linux")]
impl ManagedPlocateSearchProvider {
    pub fn system(paths: IndexPaths, logger: Arc<SearchLogger>) -> Self {
        Self {
            runner: Arc::new(SystemProcessRunner::new(logger)),
            paths,
        }
    }
}

#[cfg(target_os = "linux")]
impl SearchProvider for ManagedPlocateSearchProvider {
    fn candidates(
        &self,
        query: &str,
        whole_path: bool,
        context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError> {
        let term = longest_term(query)?;
        let databases = managed_index::database_paths(&self.paths);
        if databases.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = Vec::new();
        for database in databases {
            args.push("--database".into());
            args.push(database.to_string_lossy().into_owned());
        }
        args.push("--ignore-case".into());
        if !whole_path {
            args.push("--basename".into());
        }
        args.extend(["--existing".into(), "--".into(), term.into()]);
        let program = managed_index::plocate_path();
        let output = self
            .runner
            .run(program.to_string_lossy().as_ref(), &args, context)
            .map_err(|error| {
                SearchError::Provider(format!(
                    "Toka's private search index could not start: {error}"
                ))
            })?;
        if output.exit_code == Some(1) {
            return Ok(Vec::new());
        }
        parse_output(output, "Toka's private search failed")
    }

    fn revision(&self) -> u64 {
        managed_index::revision(&self.paths)
    }
}

fn longest_term(query: &str) -> Result<&str, SearchError> {
    query
        .split_whitespace()
        .max_by_key(|term| term.chars().count())
        .ok_or(SearchError::InvalidQuery)
}

fn parse_output(output: ProcessOutput, failure_message: &str) -> Result<Vec<PathBuf>, SearchError> {
    if !output.success {
        let detail = output.stderr.trim();
        let message = if detail.is_empty() {
            failure_message.to_owned()
        } else {
            format!("{failure_message}: {detail}")
        };
        return Err(SearchError::Provider(message));
    }

    Ok(output
        .stdout
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeRunner {
        invocations: Mutex<Vec<(String, Vec<String>)>>,
        output: ProcessOutput,
    }

    impl FakeRunner {
        fn new(output: &str) -> Self {
            Self {
                invocations: Mutex::new(Vec::new()),
                output: ProcessOutput {
                    success: true,
                    exit_code: Some(0),
                    stdout: output.into(),
                    stderr: String::new(),
                },
            }
        }

        fn no_matches() -> Self {
            Self {
                invocations: Mutex::new(Vec::new()),
                output: ProcessOutput {
                    success: false,
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: String::new(),
                },
            }
        }
    }

    impl ProcessRunner for FakeRunner {
        fn run(
            &self,
            program: &str,
            args: &[String],
            _context: &SearchLogContext,
        ) -> Result<ProcessOutput, std::io::Error> {
            self.invocations
                .lock()
                .unwrap()
                .push((program.into(), args.to_vec()));
            Ok(ProcessOutput {
                success: self.output.success,
                exit_code: self.output.exit_code,
                stdout: self.output.stdout.clone(),
                stderr: self.output.stderr.clone(),
            })
        }
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_writes_the_executed_command_to_the_search_log() {
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("search.log");
        let logger = Arc::new(SearchLogger::new(log_path.clone()));
        let runner = SystemProcessRunner::new(logger);
        let context = context("clip");

        runner.run("true", &[], &context).unwrap();

        let line = std::fs::read_to_string(log_path).unwrap();
        let entry: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(entry["kind"], "command");
        assert_eq!(entry["query"], "clip");
        assert_eq!(entry["program"], "true");
        assert_eq!(entry["command"], "true");
        assert_eq!(entry["success"], true);
    }

    fn context(query: &str) -> SearchLogContext {
        SearchLogContext {
            query: query.into(),
            page: 1,
            page_size: crate::search::PAGE_SIZE,
            fields: crate::search::SearchFields::default(),
            media_type: crate::search::MediaType::default(),
        }
    }

    #[test]
    fn mdfind_uses_display_name_movie_search_with_longest_term_and_parses_paths() {
        let runner = Arc::new(FakeRunner::new(
            "/Videos/Summer Vacation.mp4\n/Videos/another.mov\n",
        ));
        let provider = MdfindSearchProvider {
            runner: runner.clone(),
        };

        let paths = provider
            .candidates("summer vacation", false, &context("summer vacation"))
            .unwrap();

        let invocations = runner.invocations.lock().unwrap();
        assert_eq!(
            invocations.first(),
            Some(&(
                "/usr/bin/mdfind".into(),
                vec![
                    "kMDItemDisplayName == \"*vacation*\"cd && kMDItemContentTypeTree == \"public.movie\""
                        .into()
                ]
            ))
        );
        assert_eq!(paths[0], PathBuf::from("/Videos/Summer Vacation.mp4"));
    }

    #[test]
    fn recoll_uses_filename_mode_without_a_shell_and_parses_paths() {
        let runner = Arc::new(FakeRunner::new("/media/Summer Vacation.mkv\n"));
        let provider = RecollSearchProvider {
            runner: runner.clone(),
        };

        let paths = provider
            .candidates("summer vacation", false, &context("summer vacation"))
            .unwrap();

        assert_eq!(
            runner.invocations.lock().unwrap().first(),
            Some(&(
                "recollq".into(),
                vec!["-f", "-b", "--paths-only", "-C", "-n", "0", "*vacation*"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            ))
        );
        assert_eq!(paths, vec![PathBuf::from("/media/Summer Vacation.mkv")]);
    }

    #[test]
    fn downloads_fallback_finds_matching_names_recursively() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let matching = nested.join("Untitled.mov");
        let missing_term = directory.path().join("Vacation.mov");
        std::fs::write(&matching, b"test").unwrap();
        std::fs::write(&missing_term, b"test").unwrap();

        assert_eq!(
            download_folder_candidates(directory.path(), "untitled"),
            vec![matching]
        );
    }

    #[test]
    fn plocate_uses_filename_mode_without_a_shell_and_parses_paths() {
        let runner = Arc::new(FakeRunner::new("/media/Summer Vacation.mkv\n"));
        let provider = PlocateSearchProvider {
            runner: runner.clone(),
        };

        let paths = provider
            .candidates("summer vacation", false, &context("summer vacation"))
            .unwrap();

        assert_eq!(
            runner.invocations.lock().unwrap().first(),
            Some(&(
                "plocate".into(),
                [
                    "--ignore-case",
                    "--basename",
                    "--existing",
                    "--",
                    "vacation"
                ]
                .into_iter()
                .map(String::from)
                .collect()
            ))
        );
        assert_eq!(paths, vec![PathBuf::from("/media/Summer Vacation.mkv")]);
    }

    #[test]
    fn plocate_matches_whole_paths_when_the_folder_is_searched() {
        let runner = Arc::new(FakeRunner::new("/media/Holiday/clip.mkv\n"));
        let provider = PlocateSearchProvider {
            runner: runner.clone(),
        };

        provider
            .candidates("holiday", true, &context("holiday"))
            .unwrap();

        assert_eq!(
            runner.invocations.lock().unwrap().first(),
            Some(&(
                "plocate".into(),
                ["--ignore-case", "--existing", "--", "holiday"]
                    .into_iter()
                    .map(String::from)
                    .collect()
            ))
        );
    }

    #[test]
    fn mdfind_matches_whole_paths_when_the_folder_is_searched() {
        let runner = Arc::new(FakeRunner::new("/media/Holiday/clip.mkv\n"));
        let provider = MdfindSearchProvider {
            runner: runner.clone(),
        };

        provider
            .candidates("holiday", true, &context("holiday"))
            .unwrap();

        assert_eq!(
            runner.invocations.lock().unwrap().first(),
            Some(&(
                "/usr/bin/mdfind".into(),
                vec![
                    "kMDItemPath == \"*holiday*\"cd && kMDItemContentTypeTree == \"public.movie\""
                        .into()
                ]
            ))
        );
    }

    #[test]
    fn plocate_reports_no_matches_as_an_empty_result_set() {
        let provider = PlocateSearchProvider {
            runner: Arc::new(FakeRunner::no_matches()),
        };

        assert_eq!(
            provider
                .candidates("does-not-exist", false, &context("does-not-exist"),)
                .unwrap(),
            Vec::<PathBuf>::new()
        );
    }
}
