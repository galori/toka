use crate::search::{SearchError, SearchProvider};
use std::{path::PathBuf, sync::Arc};

#[cfg(not(feature = "e2e"))]
use std::process::Command;

#[derive(Debug)]
struct ProcessOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

trait ProcessRunner: Send + Sync {
    fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, std::io::Error>;
}

#[cfg(not(feature = "e2e"))]
struct SystemProcessRunner;

#[cfg(not(feature = "e2e"))]
impl ProcessRunner for SystemProcessRunner {
    fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, std::io::Error> {
        let output = Command::new(program).args(args).output()?;
        Ok(ProcessOutput {
            success: output.status.success(),
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
    #[cfg(all(target_os = "macos", not(feature = "e2e")))]
    pub fn system() -> Self {
        Self {
            runner: Arc::new(SystemProcessRunner),
        }
    }
}

#[cfg(any(target_os = "macos", test))]
impl SearchProvider for MdfindSearchProvider {
    fn candidates(&self, query: &str) -> Result<Vec<PathBuf>, SearchError> {
        let term = longest_term(query)?;
        let escaped = term.replace('\\', "\\\\").replace('"', "\\\"");
        let predicate = format!("kMDItemFSName == \"*{escaped}*\"cd");
        let output = self
            .runner
            .run("/usr/bin/mdfind", &[predicate])
            .map_err(|error| {
                SearchError::Provider(format!(
                    "Spotlight search could not start. Check macOS privacy and indexing settings: {error}"
                ))
            })?;
        parse_output(output, "Spotlight search failed")
    }
}

#[cfg(any(target_os = "linux", test))]
pub struct RecollSearchProvider {
    runner: Arc<dyn ProcessRunner>,
}

#[cfg(any(target_os = "linux", test))]
impl RecollSearchProvider {
    #[cfg(all(target_os = "linux", not(feature = "e2e")))]
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
        let escaped = term.replace('\\', "\\\\").replace('"', "\\\"");
        let filename_query = format!("filename:\"*{escaped}*\"");
        let args = ["-b", "--paths-only", "-C", "-n", "0", &filename_query]
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
        invocation: Mutex<Option<(String, Vec<String>)>>,
        output: String,
    }

    impl FakeRunner {
        fn new(output: &str) -> Self {
            Self {
                invocation: Mutex::new(None),
                output: output.into(),
            }
        }
    }

    impl ProcessRunner for FakeRunner {
        fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, std::io::Error> {
            *self.invocation.lock().unwrap() = Some((program.into(), args.to_vec()));
            Ok(ProcessOutput {
                success: true,
                stdout: self.output.clone(),
                stderr: String::new(),
            })
        }
    }

    #[test]
    fn mdfind_uses_longest_term_and_parses_paths() {
        let runner = Arc::new(FakeRunner::new(
            "/Videos/Summer Vacation.mp4\n/Videos/another.mov\n",
        ));
        let provider = MdfindSearchProvider {
            runner: runner.clone(),
        };

        let paths = provider.candidates("summer vacation").unwrap();

        assert_eq!(
            *runner.invocation.lock().unwrap(),
            Some((
                "/usr/bin/mdfind".into(),
                vec!["kMDItemFSName == \"*vacation*\"cd".into()]
            ))
        );
        assert_eq!(paths[0], PathBuf::from("/Videos/Summer Vacation.mp4"));
    }

    #[test]
    fn recoll_uses_filename_query_without_a_shell_and_parses_paths() {
        let runner = Arc::new(FakeRunner::new("/media/Summer Vacation.mkv\n"));
        let provider = RecollSearchProvider {
            runner: runner.clone(),
        };

        let paths = provider.candidates("summer vacation").unwrap();

        assert_eq!(
            *runner.invocation.lock().unwrap(),
            Some((
                "recollq".into(),
                vec![
                    "-b",
                    "--paths-only",
                    "-C",
                    "-n",
                    "0",
                    "filename:\"*vacation*\"",
                ]
                .into_iter()
                .map(String::from)
                .collect()
            ))
        );
        assert_eq!(paths, vec![PathBuf::from("/media/Summer Vacation.mkv")]);
    }
}
