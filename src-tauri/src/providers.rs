#[cfg(target_os = "linux")]
use crate::managed_index::{self, IndexPaths};
use crate::search::{tokenize_query, SearchError, SearchProvider};
use crate::search_log::SearchLogContext;
#[cfg(any(target_os = "linux", test))]
use crate::search_log::{CommandOutcome, SearchLogger};
use std::path::PathBuf;
#[cfg(any(target_os = "linux", test))]
use std::{process::Command, sync::Arc};

#[cfg(any(target_os = "linux", test))]
#[derive(Debug)]
struct ProcessOutput {
    success: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

#[cfg(any(target_os = "linux", test))]
trait ProcessRunner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        context: &SearchLogContext,
    ) -> Result<ProcessOutput, std::io::Error>;
}

#[cfg(any(target_os = "linux", test))]
struct SystemProcessRunner {
    logger: Arc<SearchLogger>,
}

#[cfg(any(target_os = "linux", test))]
impl SystemProcessRunner {
    fn new(logger: Arc<SearchLogger>) -> Self {
        Self { logger }
    }
}

#[cfg(any(target_os = "linux", test))]
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
            CommandOutcome {
                success: process_output.success,
                exit_code: process_output.exit_code,
                stdout: &process_output.stdout,
                stderr: &process_output.stderr,
            },
        );
        Ok(process_output)
    }
}

#[cfg(any(target_os = "macos", test))]
pub(crate) struct OwnedIndexSearchProvider {
    database: PathBuf,
}

#[cfg(any(target_os = "macos", test))]
impl OwnedIndexSearchProvider {
    pub(crate) fn new(database: PathBuf) -> Self {
        Self { database }
    }
}

#[cfg(any(target_os = "macos", test))]
impl SearchProvider for OwnedIndexSearchProvider {
    fn candidates(
        &self,
        query: &str,
        _whole_path: bool,
        _context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError> {
        let terms = query_terms(query)?;
        crate::owned_index::OwnedIndex::open(&self.database)
            .and_then(|index| index.search_candidates(&terms))
            .map_err(|error| SearchError::Provider(error.to_string()))
    }

    fn retain_visible_paths(&self, paths: &mut Vec<PathBuf>) -> Result<(), SearchError> {
        let roots = crate::owned_index::root_paths(&self.database)
            .map_err(|error| SearchError::Provider(error.to_string()))?;
        paths.retain(|path| {
            roots.iter().any(|root| {
                path.starts_with(root)
                    && crate::index_visibility::hidden_ancestor(root, path)
                        .is_ok_and(|hidden| hidden.is_none())
            })
        });
        Ok(())
    }

    fn revision(&self) -> u64 {
        crate::owned_index::index_revision(&self.database).unwrap_or(0)
    }
}

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
        let terms = query_terms(query)?;
        // Leading wildcard also prevents a query beginning with `-` from being
        // interpreted as another command-line option.
        let filename_query = if terms.len() == 1 && terms[0] == "*" {
            "*".into()
        } else {
            terms
                .iter()
                .map(|term| format!("*{term}*"))
                .collect::<Vec<_>>()
                .join(" OR ")
        };
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
        let terms = query_terms(query)?;
        // plocate matches the whole path unless it is confined to the base
        // name, so a folder search is the flag left off rather than added.
        let mut args = vec!["--ignore-case"];
        if !whole_path {
            args.push("--basename");
        }
        args.extend(["--existing", "--"]);
        args.extend(terms.iter().map(String::as_str));
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
        let terms = query_terms(query)?;
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
        args.extend(["--existing".into(), "--".into()]);
        args.extend(terms);
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

fn query_terms(query: &str) -> Result<Vec<String>, SearchError> {
    let tokens = tokenize_query(query)?;
    let mut terms = Vec::new();
    let mut needs_all_candidates = false;
    for token in tokens {
        if token == "(" || token == ")" || token.eq_ignore_ascii_case("AND") {
            continue;
        }
        if token.eq_ignore_ascii_case("NOT") {
            needs_all_candidates = true;
            continue;
        }
        if token.eq_ignore_ascii_case("OR") {
            continue;
        }
        if let Some((field, value)) = token.split_once(':') {
            match field.to_ascii_lowercase().as_str() {
                "size" | "created" | "modified" | "type" | "extension" | "ext" => {
                    needs_all_candidates = true;
                    continue;
                }
                "tags" | "tag" | "filename" | "file" | "name" | "path" | "folder"
                    if !value.is_empty() =>
                {
                    terms.push(value.to_owned());
                    continue;
                }
                _ => {}
            }
        }
        terms.push(token);
    }
    if needs_all_candidates {
        // A metadata or negative term can match a file regardless of its name.
        // Ask each provider for its complete indexed set, then let SearchEngine
        // apply the full expression and media filter locally.
        terms = vec!["*".into()];
    }
    if terms.is_empty() {
        Err(SearchError::InvalidQuery)
    } else {
        Ok(terms)
    }
}

#[cfg(any(target_os = "linux", test))]
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
        SearchLogContext::from(&crate::search::SearchRequest {
            query: query.into(),
            page: 1,
            page_size: crate::search::PAGE_SIZE,
            search_id: None,
            fields: crate::search::SearchFields::default(),
            media_type: crate::search::MediaType::default(),
        })
    }

    #[test]
    fn owned_index_provider_excludes_hidden_and_out_of_scope_paths() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir_all(root.join(".worktrees")).unwrap();
        let root = root.canonicalize().unwrap();
        let database = directory.path().join("index.sqlite3");
        crate::owned_index::OwnedIndexManager::under(database.clone())
            .add_folder(&root)
            .unwrap();
        let provider = OwnedIndexSearchProvider::new(database);
        let visible = root.join("clip.mp4");
        let mut paths = vec![
            visible.clone(),
            root.join(".clip.mp4"),
            root.join(".worktrees/clip.mp4"),
            directory.path().join("outside.mp4"),
        ];

        provider.retain_visible_paths(&mut paths).unwrap();

        assert_eq!(paths, vec![visible]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn owned_index_search_excludes_a_folder_hidden_after_indexing() {
        use crate::search::{MediaType, SearchEngine, SearchFields, SearchRequest, PAGE_SIZE};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("media");
        let archive = root.join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let visible = root.join("clip-visible.mp4");
        let hidden = archive.join("clip-hidden.mp4");
        std::fs::write(&visible, b"video").unwrap();
        std::fs::write(&hidden, b"video").unwrap();
        let root = root.canonicalize().unwrap();
        let database = directory.path().join("index.sqlite3");
        let state = crate::owned_index::OwnedIndexManager::under(database.clone())
            .add_folder(&root)
            .unwrap();
        crate::owned_index::OwnedIndex::open(&database)
            .unwrap()
            .scan_root(&state.folders[0].id, &root, |_| {})
            .unwrap();
        assert!(std::process::Command::new("/usr/bin/chflags")
            .arg("hidden")
            .arg(&archive)
            .status()
            .unwrap()
            .success());

        let engine = SearchEngine::new(Arc::new(OwnedIndexSearchProvider::new(database)));
        let result = engine
            .search(SearchRequest {
                query: "clip".into(),
                page: 1,
                page_size: PAGE_SIZE,
                search_id: None,
                fields: SearchFields::default(),
                media_type: MediaType::Videos,
            })
            .unwrap();

        assert_eq!(result.total_results, 1);
        assert_eq!(result.results[0].file_name, "clip-visible.mp4");
        assert!(std::process::Command::new("/usr/bin/chflags")
            .arg("nohidden")
            .arg(archive)
            .status()
            .unwrap()
            .success());
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
                vec![
                    "-f",
                    "-b",
                    "--paths-only",
                    "-C",
                    "-n",
                    "0",
                    "*summer* OR *vacation*",
                ]
                .into_iter()
                .map(String::from)
                .collect()
            ))
        );
        assert_eq!(paths, vec![PathBuf::from("/media/Summer Vacation.mkv")]);
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
                    "summer",
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
    fn plocate_keeps_special_characters_and_all_terms_in_the_query() {
        let runner = Arc::new(FakeRunner::new("/media/@ 12 [clip].mkv\n"));
        let provider = PlocateSearchProvider {
            runner: runner.clone(),
        };

        provider
            .candidates("@ 12 [", false, &context("@ 12 ["))
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
                    "@",
                    "12",
                    "["
                ]
                .into_iter()
                .map(String::from)
                .collect()
            ))
        );
    }

    #[test]
    fn providers_keep_quoted_values_together() {
        assert_eq!(
            query_terms("filename:\"family trip\" OR folder:archive").unwrap(),
            ["family trip", "archive"]
        );
    }

    #[test]
    fn metadata_and_negative_terms_request_every_indexed_path() {
        for query in [
            "type:image",
            "extension:jpg",
            "NOT tags:hidden",
            "filename:trip OR NOT folder:private",
        ] {
            assert_eq!(query_terms(query).unwrap(), ["*"], "query: {query}");
        }
    }

    #[test]
    fn providers_search_the_value_of_a_qualified_term() {
        let runner = Arc::new(FakeRunner::new("/media/Holiday/clip.mkv\n"));
        let provider = PlocateSearchProvider {
            runner: runner.clone(),
        };

        provider
            .candidates(
                "tags:home OR filename:clip OR path:Holiday",
                true,
                &context("tags:home OR filename:clip OR path:Holiday"),
            )
            .unwrap();

        assert_eq!(
            runner.invocations.lock().unwrap().first(),
            Some(&(
                "plocate".into(),
                [
                    "--ignore-case",
                    "--existing",
                    "--",
                    "home",
                    "clip",
                    "Holiday",
                ]
                .into_iter()
                .map(String::from)
                .collect()
            ))
        );
    }

    #[test]
    fn metadata_qualifiers_request_all_indexed_paths_for_local_filtering() {
        let runner = Arc::new(FakeRunner::new("/media/Holiday/clip.mkv\n"));
        let provider = PlocateSearchProvider {
            runner: runner.clone(),
        };

        provider
            .candidates(
                "created:>1-1-2026 filename:clip",
                false,
                &context("created:>1-1-2026 filename:clip"),
            )
            .unwrap();

        assert_eq!(
            runner.invocations.lock().unwrap().first(),
            Some(&(
                "plocate".into(),
                ["--ignore-case", "--basename", "--existing", "--", "*"]
                    .into_iter()
                    .map(String::from)
                    .collect()
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
