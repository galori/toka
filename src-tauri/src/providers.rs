#[cfg(target_os = "linux")]
use crate::managed_index::{self, IndexPaths};
use crate::search::{tokenize_query, SearchError, SearchProvider};
use crate::search_log::{CommandOutcome, SearchLogContext, SearchLogger};
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
        let predicate = mdfind_predicate(query, whole_path)?;
        let mut paths = run_mdfind(&*self.runner, vec![predicate], context)?;
        paths.extend(macos_download_candidates(query)?);
        Ok(paths)
    }
}

#[cfg(any(target_os = "macos", test))]
fn mdfind_predicate(query: &str, whole_path: bool) -> Result<String, SearchError> {
    let terms = query_terms(query)?;
    // Spotlight's display name is the file's own name; its path attribute
    // covers the folders above it, which is what a folder search needs.
    let attribute = if whole_path {
        "kMDItemPath"
    } else {
        "kMDItemDisplayName"
    };
    let term_predicates = terms
        .iter()
        .map(|term| {
            if term == "*" {
                return format!("{attribute} == \"*\"cd");
            }
            let escaped = term.replace('\\', "\\\\").replace('"', "\\\"");
            format!("{attribute} == \"*{escaped}*\"cd")
        })
        .collect::<Vec<_>>();
    let matching = if term_predicates.len() == 1 {
        term_predicates[0].clone()
    } else {
        format!("({})", term_predicates.join(" || "))
    };
    let media_predicate = "(kMDItemContentTypeTree == \"public.movie\" || kMDItemContentTypeTree == \"public.image\")";
    Ok(format!("{matching} && {media_predicate}"))
}

#[cfg(any(target_os = "macos", test))]
pub struct ScopedMdfindSearchProvider {
    runner: Arc<dyn ProcessRunner>,
    database: PathBuf,
}

#[cfg(any(target_os = "macos", test))]
impl ScopedMdfindSearchProvider {
    #[cfg(target_os = "macos")]
    pub fn system(logger: Arc<SearchLogger>, database: PathBuf) -> Self {
        Self {
            runner: MdfindSearchProvider::system(logger).runner,
            database,
        }
    }
}

#[cfg(any(target_os = "macos", test))]
impl SearchProvider for ScopedMdfindSearchProvider {
    fn candidates(
        &self,
        query: &str,
        whole_path: bool,
        context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError> {
        let roots = crate::owned_index::root_paths(&self.database)
            .map_err(|error| SearchError::Provider(error.to_string()))?;
        if roots.is_empty() {
            return MdfindSearchProvider {
                runner: self.runner.clone(),
            }
            .candidates(query, whole_path, context);
        }

        let predicate = mdfind_predicate(query, whole_path)?;
        let mut paths = Vec::new();
        for root in &roots {
            paths.extend(run_mdfind(
                &*self.runner,
                vec![
                    "-onlyin".into(),
                    root.to_string_lossy().into_owned(),
                    predicate.clone(),
                ],
                context,
            )?);
        }
        paths.extend(macos_download_candidates_in_roots(query, &roots)?);
        Ok(paths)
    }

    fn retain_visible_paths(&self, paths: &mut Vec<PathBuf>) -> Result<(), SearchError> {
        let mut roots = crate::owned_index::root_paths(&self.database)
            .map_err(|error| SearchError::Provider(error.to_string()))?;
        if roots.is_empty() {
            roots.push(PathBuf::from("/"));
        }
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
    let Ok(terms) = query_terms(query) else {
        return Vec::new();
    };
    let terms = terms
        .into_iter()
        .map(|term| term.to_lowercase())
        .collect::<Vec<_>>();
    let mut candidates = Vec::new();
    let mut pending = vec![root.to_path_buf()];

    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if crate::index_visibility::is_hidden(&path).unwrap_or(true) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
            } else {
                let haystack = path.to_string_lossy().to_lowercase();
                if terms
                    .iter()
                    .any(|term| term == "*" || haystack.contains(term))
                {
                    candidates.push(path);
                }
            }
        }
    }

    candidates
}

#[cfg(any(target_os = "macos", test))]
fn download_folder_candidates_in_roots(
    downloads: &Path,
    query: &str,
    roots: &[PathBuf],
) -> Vec<PathBuf> {
    roots
        .iter()
        .flat_map(|root| {
            let search_root = if root.starts_with(downloads) {
                root.as_path()
            } else if downloads.starts_with(root) {
                downloads
            } else {
                return Vec::new();
            };
            download_folder_candidates(search_root, query)
                .into_iter()
                .filter(|path| path.starts_with(root))
                .collect()
        })
        .collect()
}

#[cfg(all(target_os = "macos", not(test)))]
fn macos_download_candidates_in_roots(
    query: &str,
    roots: &[PathBuf],
) -> Result<Vec<PathBuf>, SearchError> {
    let Some(home) = std::env::var_os("HOME") else {
        return Ok(Vec::new());
    };
    let downloads = PathBuf::from(home).join("Downloads");
    if !roots
        .iter()
        .any(|root| root.starts_with(&downloads) || downloads.starts_with(root))
    {
        return Ok(Vec::new());
    }
    prepare_macos_downloads_folder_access();
    Ok(download_folder_candidates_in_roots(
        &downloads, query, roots,
    ))
}

#[cfg(test)]
fn macos_download_candidates_in_roots(
    _query: &str,
    _roots: &[PathBuf],
) -> Result<Vec<PathBuf>, SearchError> {
    Ok(Vec::new())
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
    fn mdfind_uses_display_name_movie_search_for_all_terms_and_parses_paths() {
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
                    "(kMDItemDisplayName == \"*summer*\"cd || kMDItemDisplayName == \"*vacation*\"cd) && (kMDItemContentTypeTree == \"public.movie\" || kMDItemContentTypeTree == \"public.image\")"
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
    fn downloads_fallback_finds_matching_names_recursively() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let matching = nested.join("Untitled.mov");
        let missing_term = directory.path().join("Vacation.mov");
        std::fs::write(&matching, b"test").unwrap();
        std::fs::write(&missing_term, b"test").unwrap();
        std::fs::create_dir(nested.join(".private")).unwrap();
        std::fs::write(nested.join(".Untitled.mov"), b"test").unwrap();
        std::fs::write(nested.join(".private/Untitled.mov"), b"test").unwrap();

        assert_eq!(
            download_folder_candidates(directory.path(), "untitled"),
            vec![matching]
        );
    }

    #[test]
    fn downloads_fallback_widens_folder_and_metadata_queries() {
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("Family Archive");
        std::fs::create_dir(&archive).unwrap();
        let photo = archive.join("Trip.jpg");
        let video = directory.path().join("Clip.mov");
        std::fs::write(&photo, b"test").unwrap();
        std::fs::write(&video, b"test").unwrap();

        assert_eq!(
            download_folder_candidates(directory.path(), "folder:archive"),
            vec![photo.clone()]
        );
        let mut metadata = download_folder_candidates(directory.path(), "type:image");
        metadata.sort();
        let mut expected = vec![photo, video];
        expected.sort();
        assert_eq!(metadata, expected);
    }

    #[test]
    fn downloads_fallback_can_be_limited_to_configured_roots() {
        let directory = tempfile::tempdir().unwrap();
        let downloads = directory.path().join("Downloads");
        let included_root = downloads.join("Videos");
        let excluded_root = downloads.join("Unconfigured");
        std::fs::create_dir_all(&included_root).unwrap();
        std::fs::create_dir_all(&excluded_root).unwrap();
        let included = included_root.join("clip.mov");
        std::fs::write(&included, b"video").unwrap();
        std::fs::write(excluded_root.join("clip.mov"), b"video").unwrap();

        assert_eq!(
            download_folder_candidates_in_roots(
                &downloads,
                "clip",
                std::slice::from_ref(&included_root),
            ),
            vec![included]
        );
    }

    #[test]
    fn mdfind_searches_images_and_videos_for_local_type_filtering() {
        let runner = Arc::new(FakeRunner::new("/Pictures/Trip.jpg\n"));
        let provider = MdfindSearchProvider {
            runner: runner.clone(),
        };

        provider
            .candidates("type:image", false, &context("type:image"))
            .unwrap();

        assert_eq!(
            runner.invocations.lock().unwrap().first(),
            Some(&(
                "/usr/bin/mdfind".into(),
                vec![
                    "kMDItemDisplayName == \"*\"cd && (kMDItemContentTypeTree == \"public.movie\" || kMDItemContentTypeTree == \"public.image\")"
                        .into()
                ]
            ))
        );
    }

    #[test]
    fn scoped_search_excludes_hidden_files_and_folders() {
        use crate::search::{MediaType, SearchEngine, SearchFields, SearchRequest, PAGE_SIZE};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir_all(root.join(".worktrees")).unwrap();
        std::fs::create_dir_all(root.join("archive")).unwrap();
        let root = root.canonicalize().unwrap();
        let visible = root.join("clip.mp4");
        let hidden_file = root.join(".clip.mp4");
        let hidden_child = root.join(".worktrees/clip.mp4");
        let archive = root.join("archive/clip.mp4");
        for path in [&visible, &hidden_file, &hidden_child, &archive] {
            std::fs::write(path, b"video").unwrap();
        }
        #[cfg(target_os = "macos")]
        assert!(Command::new("/usr/bin/chflags")
            .arg("hidden")
            .arg(root.join("archive"))
            .status()
            .unwrap()
            .success());
        let database = directory.path().join("index.sqlite3");
        crate::owned_index::OwnedIndexManager::under(database.clone())
            .add_folder(&root)
            .unwrap();
        let output = [&visible, &hidden_file, &hidden_child, &archive]
            .map(|path| path.to_string_lossy())
            .join("\n");
        let engine = SearchEngine::new(Arc::new(ScopedMdfindSearchProvider {
            runner: Arc::new(FakeRunner::new(&output)),
            database,
        }));
        // A recent folder rename adds candidates after the provider query.
        engine.rename_folder(&root.join("previous"), root.join("archive"));

        let results = engine
            .search(SearchRequest {
                query: "clip".into(),
                page: 1,
                page_size: PAGE_SIZE,
                fields: SearchFields::default(),
                media_type: MediaType::Both,
            })
            .unwrap();

        assert_eq!(
            results.total_results,
            if cfg!(target_os = "macos") { 1 } else { 2 }
        );
        assert!(results
            .results
            .iter()
            .all(|result| result.file_name == "clip.mp4"));
    }

    #[test]
    fn scoped_mdfind_only_searches_configured_folders() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("media");
        std::fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let database = directory.path().join("index.sqlite3");
        let manager = crate::owned_index::OwnedIndexManager::under(database.clone());
        manager.add_folder(&root).unwrap();
        let clip = root.join("clip.mp4");
        let runner = Arc::new(FakeRunner::new(clip.to_string_lossy().as_ref()));
        let provider = ScopedMdfindSearchProvider {
            runner: runner.clone(),
            database,
        };

        let paths = provider
            .candidates("clip", false, &context("clip"))
            .unwrap();

        assert_eq!(
            runner.invocations.lock().unwrap().first(),
            Some(&(
                "/usr/bin/mdfind".into(),
                vec![
                    "-onlyin".into(),
                    root.to_string_lossy().into_owned(),
                    mdfind_predicate("clip", false).unwrap(),
                ]
            ))
        );
        assert_eq!(paths, vec![clip]);
    }

    #[test]
    fn scoped_mdfind_falls_back_to_unscoped_search_without_folders() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        let runner = Arc::new(FakeRunner::new(
            "/media/clip.mp4
",
        ));
        let provider = ScopedMdfindSearchProvider {
            runner: runner.clone(),
            database,
        };

        provider
            .candidates("clip", false, &context("clip"))
            .unwrap();

        assert_eq!(
            runner.invocations.lock().unwrap().first(),
            Some(&(
                "/usr/bin/mdfind".into(),
                vec![mdfind_predicate("clip", false).unwrap()]
            ))
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
                    "kMDItemPath == \"*holiday*\"cd && (kMDItemContentTypeTree == \"public.movie\" || kMDItemContentTypeTree == \"public.image\")"
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
