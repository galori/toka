use crate::search::{MediaType, SearchFields, SearchRequest};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::{self, create_dir_all, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone)]
pub(crate) struct SearchLogContext {
    pub(crate) query: String,
    pub(crate) page: usize,
    pub(crate) page_size: usize,
    pub(crate) fields: SearchFields,
    pub(crate) media_type: MediaType,
    verbose: Option<Arc<VerboseSearchLog>>,
}

impl From<&SearchRequest> for SearchLogContext {
    fn from(request: &SearchRequest) -> Self {
        Self {
            query: request.query.clone(),
            page: request.page,
            page_size: request.page_size,
            fields: request.fields,
            media_type: request.media_type,
            verbose: None,
        }
    }
}

pub(crate) struct CommandOutcome<'a> {
    pub(crate) success: bool,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: &'a str,
    pub(crate) stderr: &'a str,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchLogSettings {
    pub(crate) verbose: bool,
    pub(crate) path: Option<PathBuf>,
}

pub(crate) struct SearchLogger {
    path: PathBuf,
    settings_path: Option<PathBuf>,
    settings: Mutex<SearchLogSettings>,
    write_lock: Mutex<()>,
    // Dropping this marker is what lets the last Toka process clean up the
    // session's JSON log without disturbing another open process.
    _session: Option<SessionMarker>,
}

impl SearchLogger {
    #[cfg(test)]
    pub(crate) fn new(path: PathBuf) -> Self {
        Self::with_settings(path, None, SearchLogSettings::default(), None)
    }

    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self::new(PathBuf::new())
    }

    pub(crate) fn system() -> Self {
        #[cfg(target_os = "macos")]
        let path = dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Library/Logs/app.toka.desktop/search.log");
        #[cfg(not(target_os = "macos"))]
        let path = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("app.toka.desktop/logs/search.log");
        let settings_path = path.with_file_name("search-log-settings.json");
        let settings = load_settings(&settings_path);
        let session = SessionMarker::register(&path);
        Self::with_settings(path, Some(settings_path), settings, session)
    }

    #[cfg(test)]
    fn with_settings_file(path: PathBuf, settings_path: PathBuf) -> Self {
        Self::with_settings(
            path,
            Some(settings_path),
            SearchLogSettings::default(),
            None,
        )
    }

    fn with_settings(
        path: PathBuf,
        settings_path: Option<PathBuf>,
        settings: SearchLogSettings,
        session: Option<SessionMarker>,
    ) -> Self {
        Self {
            path,
            settings_path,
            settings: Mutex::new(settings),
            write_lock: Mutex::new(()),
            _session: session,
        }
    }

    pub(crate) fn settings(&self) -> SearchLogSettings {
        self.settings.lock().unwrap().clone()
    }

    pub(crate) fn set_settings(
        &self,
        verbose: bool,
        path: Option<PathBuf>,
    ) -> Result<SearchLogSettings, String> {
        if verbose {
            let directory = path
                .as_deref()
                .ok_or_else(|| "Choose a folder for verbose search logs.".to_owned())?;
            validate_log_directory(directory)?;
        }
        let next = SearchLogSettings { verbose, path };
        let Some(settings_path) = &self.settings_path else {
            return Err("Search log settings are unavailable in this build.".into());
        };
        write_settings(settings_path, &next)?;
        *self.settings.lock().unwrap() = next.clone();
        Ok(next)
    }

    pub(crate) fn context(
        &self,
        request: &SearchRequest,
        with_verbose_log: bool,
    ) -> SearchLogContext {
        let mut context = SearchLogContext::from(request);
        if with_verbose_log {
            let settings = self.settings();
            if settings.verbose {
                context.verbose = settings
                    .path
                    .as_deref()
                    .and_then(|directory| create_verbose_log(directory, &context));
            }
        }
        context
    }

    pub(crate) fn record_command(
        &self,
        context: &SearchLogContext,
        program: &str,
        args: &[String],
        outcome: CommandOutcome<'_>,
    ) {
        let mut entry = context_entry(context, "command");
        entry.insert("program".into(), json!(program));
        entry.insert("args".into(), json!(args));
        entry.insert("command".into(), json!(display_command(program, args)));
        entry.insert("success".into(), json!(outcome.success));
        entry.insert("exitCode".into(), json!(outcome.exit_code));
        if !outcome.stderr.trim().is_empty() {
            entry.insert("stderr".into(), json!(outcome.stderr.trim()));
        }
        self.write(Value::Object(entry));
        if let Some(verbose) = &context.verbose {
            verbose.record_command(
                program,
                args,
                outcome.success,
                outcome.exit_code,
                outcome.stdout,
                outcome.stderr,
            );
        }
    }

    pub(crate) fn record_filtered(&self, context: &SearchLogContext, path: &Path, reason: &str) {
        if let Some(verbose) = &context.verbose {
            verbose.record_filtered(path, reason);
        }
    }

    pub(crate) fn record_returned(&self, context: &SearchLogContext, paths: &[PathBuf]) {
        if let Some(verbose) = &context.verbose {
            verbose.record_returned(paths);
        }
    }

    pub(crate) fn record_error(&self, context: &SearchLogContext, error: &str) {
        let mut entry = context_entry(context, "error");
        entry.insert("error".into(), json!(error));
        self.write(Value::Object(entry));
        if let Some(verbose) = &context.verbose {
            verbose.record_error(error);
        }
    }

    fn write(&self, entry: Value) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let _guard = self.write_lock.lock().unwrap();
        let Some(parent) = self.path.parent() else {
            return;
        };
        if let Err(error) = create_dir_all(parent) {
            eprintln!("Could not create Toka search log directory: {error}");
            return;
        }
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            eprintln!("Could not open Toka search log: {}", self.path.display());
            return;
        };
        let Ok(line) = serde_json::to_string(&entry) else {
            eprintln!("Could not serialize a Toka search log entry");
            return;
        };
        if let Err(error) = writeln!(file, "{line}") {
            eprintln!("Could not write Toka search log: {error}");
        }
    }
}

fn context_entry(context: &SearchLogContext, kind: &str) -> serde_json::Map<String, Value> {
    let mut entry = serde_json::Map::new();
    entry.insert("kind".into(), json!(kind));
    entry.insert("timestamp".into(), json!(now_seconds()));
    entry.insert("query".into(), json!(context.query));
    entry.insert("page".into(), json!(context.page));
    entry.insert("pageSize".into(), json!(context.page_size));
    entry.insert(
        "fields".into(),
        json!({
            "tags": context.fields.tags,
            "fileName": context.fields.file_name,
            "path": context.fields.path,
        }),
    );
    entry.insert("mediaType".into(), json!(context.media_type));
    entry
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn load_settings(path: &Path) -> SearchLogSettings {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => SearchLogSettings::default(),
    }
}

fn write_settings(path: &Path, settings: &SearchLogSettings) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Toka's search log settings have no parent folder.".to_owned())?;
    create_dir_all(parent).map_err(|error| {
        format!("Toka could not create the search log settings folder: {error}")
    })?;
    let temporary = path.with_extension("json.new");
    let body = serde_json::to_vec_pretty(settings)
        .map_err(|error| format!("Toka could not encode search log settings: {error}"))?;
    fs::write(&temporary, body)
        .and_then(|_| fs::rename(&temporary, path))
        .map_err(|error| format!("Toka could not save search log settings: {error}"))
}

fn validate_log_directory(path: &Path) -> Result<(), String> {
    if !path.is_dir() {
        return Err("Choose an existing folder for verbose search logs.".into());
    }
    let probe = path.join(format!(
        ".toka-search-log-write-test-{}-{}",
        std::process::id(),
        VERBOSE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|error| format!("Toka cannot write verbose search logs there: {error}"))?;
    fs::remove_file(&probe)
        .map_err(|error| format!("Toka could not clean up its log-folder check: {error}"))
}

static VERBOSE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct VerboseSearchLog {
    file: Mutex<fs::File>,
}

fn create_verbose_log(
    directory: &Path,
    context: &SearchLogContext,
) -> Option<Arc<VerboseSearchLog>> {
    if !directory.is_dir() {
        return None;
    }
    for _ in 0..10 {
        let name = format!(
            "toka_search_log_{}_{}_{}.log",
            now_millis(),
            std::process::id(),
            VERBOSE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = directory.join(name);
        let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(path) else {
            continue;
        };
        if write_verbose_header(&mut file, context).is_err() {
            continue;
        }
        return Some(Arc::new(VerboseSearchLog {
            file: Mutex::new(file),
        }));
    }
    eprintln!(
        "Toka could not create a verbose search log in {}",
        directory.display()
    );
    None
}

fn write_verbose_header(file: &mut fs::File, context: &SearchLogContext) -> std::io::Result<()> {
    writeln!(file, "Toka verbose search log")?;
    writeln!(file, "Started: {}", now_seconds())?;
    writeln!(file, "Query: {}", context.query)?;
    writeln!(file, "Page: {}", context.page)?;
    writeln!(file, "Page size: {}", context.page_size)?;
    writeln!(file, "Media type: {:?}", context.media_type)?;
    writeln!(
        file,
        "Fields: tags={}, file name={}, path={}",
        context.fields.tags, context.fields.file_name, context.fields.path
    )?;
    writeln!(file)?;
    Ok(())
}

impl VerboseSearchLog {
    fn record_command(
        &self,
        program: &str,
        args: &[String],
        success: bool,
        exit_code: Option<i32>,
        stdout: &str,
        stderr: &str,
    ) {
        let mut file = self.file.lock().unwrap();
        let _ = writeln!(file, "COMMAND: {}", display_command(program, args));
        let _ = writeln!(file, "Succeeded: {success}");
        let _ = writeln!(file, "Exit code: {exit_code:?}");
        write_verbose_block(&mut file, "PLOCATE RESULTS", stdout);
        if !stderr.trim().is_empty() {
            write_verbose_block(&mut file, "COMMAND STDERR", stderr);
        }
        let _ = writeln!(file);
    }

    fn record_filtered(&self, path: &Path, reason: &str) {
        let mut file = self.file.lock().unwrap();
        let _ = writeln!(file, "FILTERED OUT: {} ({reason})", path.display());
    }

    fn record_returned(&self, paths: &[PathBuf]) {
        let mut file = self.file.lock().unwrap();
        let _ = writeln!(file, "RESULTS RETURNED TO APP:");
        if paths.is_empty() {
            let _ = writeln!(file, "(none)");
        } else {
            for path in paths {
                let _ = writeln!(file, "{}", path.display());
            }
        }
        let _ = writeln!(file);
    }

    fn record_error(&self, error: &str) {
        let mut file = self.file.lock().unwrap();
        let _ = writeln!(file, "SEARCH ERROR: {error}");
    }
}

fn write_verbose_block(file: &mut fs::File, title: &str, value: &str) {
    let _ = writeln!(file, "{title}:");
    if value.is_empty() {
        let _ = writeln!(file, "(empty)");
    } else {
        let _ = write!(file, "{value}");
        if !value.ends_with('\n') {
            let _ = writeln!(file);
        }
    }
}

struct SessionMarker {
    marker_path: PathBuf,
    marker_dir: PathBuf,
    log_path: PathBuf,
}

impl SessionMarker {
    fn register(log_path: &Path) -> Option<Self> {
        let marker_dir = log_path.parent()?.join(".sessions");
        create_dir_all(&marker_dir).ok()?;
        cleanup_stale_sessions(&marker_dir);
        let marker_path = marker_dir.join(format!(
            "{}-{}.session",
            std::process::id(),
            VERBOSE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&marker_path, std::process::id().to_string()).ok()?;
        Some(Self {
            marker_path,
            marker_dir,
            log_path: log_path.to_path_buf(),
        })
    }

    #[cfg(test)]
    fn register_for_test(log_path: &Path, marker_dir: &Path, name: &str) -> Self {
        create_dir_all(marker_dir).unwrap();
        let marker_path = marker_dir.join(format!("{name}.session"));
        fs::write(&marker_path, std::process::id().to_string()).unwrap();
        Self {
            marker_path,
            marker_dir: marker_dir.to_path_buf(),
            log_path: log_path.to_path_buf(),
        }
    }
}

impl Drop for SessionMarker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.marker_path);
        cleanup_stale_sessions(&self.marker_dir);
        if !has_live_session(&self.marker_dir) {
            let _ = fs::remove_file(&self.log_path);
        }
    }
}

fn cleanup_stale_sessions(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("session") {
            continue;
        }
        let Ok(pid) = fs::read_to_string(&path)
            .ok()
            .and_then(|value| value.trim().parse::<u32>().ok())
            .ok_or(())
        else {
            continue;
        };
        if !process_is_alive(pid) {
            let _ = fs::remove_file(path);
        }
    }
}

fn has_live_session(directory: &Path) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.extension().and_then(|extension| extension.to_str()) == Some("session")
            && fs::read_to_string(path)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
                .is_some_and(process_is_alive)
    })
}

fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn display_command(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-._/:@".contains(character))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn context() -> SearchLogContext {
        SearchLogContext {
            query: "summer vacation".into(),
            page: 1,
            page_size: 24,
            fields: SearchFields {
                tags: true,
                file_name: true,
                path: false,
            },
            media_type: MediaType::Videos,
            verbose: None,
        }
    }

    #[test]
    fn records_search_parameters_and_the_exact_command() {
        let directory = tempfile::tempdir().unwrap();
        let logger = SearchLogger::new(directory.path().join("search.log"));
        let context = context();

        logger.record_command(
            &context,
            "plocate",
            &[
                "--ignore-case".into(),
                "--basename".into(),
                "--existing".into(),
                "--".into(),
                "vacation".into(),
            ],
            CommandOutcome {
                success: true,
                exit_code: Some(0),
                stdout: "/Videos/vacation.mp4\n",
                stderr: "",
            },
        );

        let line = fs::read_to_string(directory.path().join("search.log")).unwrap();
        let entry: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(entry["kind"], "command");
        assert_eq!(entry["query"], "summer vacation");
        assert_eq!(entry["page"], 1);
        assert_eq!(entry["pageSize"], 24);
        assert_eq!(entry["fields"]["tags"], true);
        assert_eq!(entry["fields"]["fileName"], true);
        assert_eq!(entry["fields"]["path"], false);
        assert_eq!(entry["mediaType"], "videos");
        assert_eq!(entry["program"], "plocate");
        assert_eq!(
            entry["command"],
            "plocate --ignore-case --basename --existing -- vacation"
        );
        assert_eq!(entry["exitCode"], 0);
        assert!(entry["timestamp"].as_u64().is_some());
    }

    #[test]
    fn verbose_logs_contain_the_command_results_filter_reasons_and_returned_paths() {
        let directory = tempfile::tempdir().unwrap();
        let logger = SearchLogger::with_settings_file(
            directory.path().join("search.log"),
            directory.path().join("settings.json"),
        );
        logger
            .set_settings(true, Some(directory.path().to_path_buf()))
            .unwrap();
        let context = logger.context(
            &SearchRequest {
                query: "summer".into(),
                page: 1,
                page_size: 24,
                fields: SearchFields::default(),
                media_type: MediaType::Videos,
            },
            true,
        );
        logger.record_command(
            &context,
            "plocate",
            &["--".into(), "summer".into()],
            CommandOutcome {
                success: true,
                exit_code: Some(0),
                stdout: "/Videos/summer.mp4\n",
                stderr: "",
            },
        );
        logger.record_filtered(&context, Path::new("/Videos/empty.mp4"), "empty file");
        logger.record_returned(&context, &[PathBuf::from("/Videos/summer.mp4")]);

        let logs = fs::read_dir(directory.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.starts_with("toka_search_log_"))
            })
            .collect::<Vec<_>>();
        assert_eq!(logs.len(), 1);
        let body = fs::read_to_string(&logs[0]).unwrap();
        assert!(body.contains("COMMAND: plocate -- summer"));
        assert!(body.contains("/Videos/summer.mp4"));
        assert!(body.contains("FILTERED OUT: /Videos/empty.mp4 (empty file)"));
        assert!(body.contains("RESULTS RETURNED TO APP:"));
    }

    #[test]
    fn records_search_errors_without_failing_the_caller() {
        let directory = tempfile::tempdir().unwrap();
        let logger = SearchLogger::new(directory.path().join("search.log"));
        let context = SearchLogContext {
            query: "broken".into(),
            page: 1,
            page_size: 24,
            fields: SearchFields::default(),
            media_type: MediaType::Videos,
            verbose: None,
        };

        logger.record_error(&context, "plocate failed");

        let line = fs::read_to_string(directory.path().join("search.log")).unwrap();
        let entry: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(entry["kind"], "error");
        assert_eq!(entry["query"], "broken");
        assert_eq!(entry["error"], "plocate failed");
    }

    #[test]
    fn verbose_settings_require_an_existing_writable_directory() {
        let directory = tempfile::tempdir().unwrap();
        let logger = SearchLogger::with_settings_file(
            directory.path().join("search.log"),
            directory.path().join("settings.json"),
        );
        assert!(logger.set_settings(true, None).is_err());
        assert!(logger
            .set_settings(true, Some(directory.path().join("missing")))
            .is_err());
        assert!(logger
            .set_settings(true, Some(directory.path().to_path_buf()))
            .is_ok());
        assert!(logger.settings().verbose);
    }

    #[test]
    fn deleting_one_session_keeps_the_shared_log_until_the_last_session_closes() {
        let directory = tempfile::tempdir().unwrap();
        let log_path = directory.path().join("search.log");
        let marker_dir = directory.path().join("sessions");
        fs::write(&log_path, "search\n").unwrap();
        let first = SessionMarker::register_for_test(&log_path, &marker_dir, "first");
        let second = SessionMarker::register_for_test(&log_path, &marker_dir, "second");

        drop(first);
        assert!(log_path.is_file());
        drop(second);
        assert!(!log_path.exists());
    }
}
