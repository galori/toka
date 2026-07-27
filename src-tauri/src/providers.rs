use crate::search::{SearchError, SearchProvider};
#[cfg(all(target_os = "macos", not(test)))]
use std::sync::Once;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use std::process::Command;

#[derive(Debug)]
struct ProcessOutput {
    success: bool,
    #[cfg(any(target_os = "linux", test))]
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
}

trait ProcessRunner: Send + Sync {
    fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, std::io::Error>;
}

struct SystemProcessRunner;

impl ProcessRunner for SystemProcessRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, std::io::Error> {
        let output = Command::new(program).args(args).output()?;
        Ok(ProcessOutput {
            success: output.status.success(),
            #[cfg(any(target_os = "linux", test))]
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(any(target_os = "macos", test))]
pub struct MdfindSearchProvider {
    runner: Arc<dyn ProcessRunner>,
}

#[cfg(any(target_os = "macos", test))]
impl MdfindSearchProvider {
    #[cfg(target_os = "macos")]
    pub fn system() -> Self {
        Self {
            runner: Arc::new(SystemProcessRunner),
        }
    }
}

#[cfg(any(target_os = "macos", test))]
impl SearchProvider for MdfindSearchProvider {
    fn candidates(&self, query: &str) -> Result<Vec<PathBuf>, SearchError> {
        prepare_macos_downloads_folder_access();
        let term = longest_term(query)?;
        let escaped = term.replace('\\', "\\\\").replace('"', "\\\"");
        let predicate = format!(
            "kMDItemDisplayName == \"*{escaped}*\"cd && kMDItemContentTypeTree == \"public.movie\""
        );
        let mut paths = run_mdfind(&*self.runner, vec![predicate])?;
        paths.extend(macos_download_candidates(query)?);
        Ok(paths)
    }
}

#[cfg(any(target_os = "macos", test))]
fn run_mdfind(runner: &dyn ProcessRunner, args: Vec<String>) -> Result<Vec<PathBuf>, SearchError> {
    let output = runner.run("/usr/bin/mdfind", &args).map_err(|error| {
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

#[cfg(any(not(target_os = "macos"), test))]
fn macos_download_candidates(_query: &str) -> Result<Vec<PathBuf>, SearchError> {
    Ok(Vec::new())
}

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

#[cfg(any(not(target_os = "macos"), test))]
fn prepare_macos_downloads_folder_access() {}

#[cfg(any(target_os = "linux", test))]
pub struct RecollSearchProvider {
    runner: Arc<dyn ProcessRunner>,
}

#[cfg(any(target_os = "linux", test))]
impl RecollSearchProvider {
    #[cfg(target_os = "linux")]
    pub fn system() -> Self {
        Self {
            runner: Arc::new(SystemProcessRunner),
        }
    }
}

#[cfg(any(target_os = "linux", test))]
impl SearchProvider for RecollSearchProvider {
    fn candidates(&self, query: &str) -> Result<Vec<PathBuf>, SearchError> {
        let term = longest_term(query)?;
        // Leading wildcard also prevents a query beginning with `-` from being
        // interpreted as another command-line option.
        let filename_query = format!("*{term}*");
        let args = ["-f", "-b", "--paths-only", "-C", "-n", "0", &filename_query]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let output = self.runner.run("recollq", &args).map_err(|error| {
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
    pub fn system() -> Self {
        Self {
            runner: Arc::new(SystemProcessRunner),
        }
    }
}

#[cfg(any(target_os = "linux", test))]
impl SearchProvider for PlocateSearchProvider {
    fn candidates(&self, query: &str) -> Result<Vec<PathBuf>, SearchError> {
        let term = longest_term(query)?;
        let args = ["--ignore-case", "--basename", "--existing", "--", term]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let output = self.runner.run("plocate", &args).map_err(|error| SearchError::Provider(format!("plocate search could not start. Install plocate and build its index with updatedb: {error}")))?;
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
        fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, std::io::Error> {
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

    #[test]
    fn mdfind_uses_display_name_movie_search_with_longest_term_and_parses_paths() {
        let runner = Arc::new(FakeRunner::new(
            "/Videos/Summer Vacation.mp4\n/Videos/another.mov\n",
        ));
        let provider = MdfindSearchProvider {
            runner: runner.clone(),
        };

        let paths = provider.candidates("summer vacation").unwrap();

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

        let paths = provider.candidates("summer vacation").unwrap();

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

        let paths = provider.candidates("summer vacation").unwrap();

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
    fn plocate_reports_no_matches_as_an_empty_result_set() {
        let provider = PlocateSearchProvider {
            runner: Arc::new(FakeRunner::no_matches()),
        };

        assert_eq!(
            provider.candidates("does-not-exist").unwrap(),
            Vec::<PathBuf>::new()
        );
    }
}
