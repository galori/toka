use crate::search::{MediaType, SearchFields, SearchRequest, SearchSort};
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
pub struct SearchLogContext {
    pub(crate) query: String,
    pub(crate) page: usize,
    pub(crate) page_size: usize,
    pub(crate) fields: SearchFields,
    pub(crate) media_type: MediaType,
    pub(crate) sort: SearchSort,
    pub(crate) addon_id: Option<String>,
    pub(crate) addon_capture: Option<String>,
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
            sort: request.sort,
            addon_id: None,
            addon_capture: None,
            verbose: None,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SearchLogSettings {
    pub(crate) verbose: bool,
    pub(crate) path: Option<PathBuf>,
}

pub struct SearchLogger {
    path: PathBuf,
    settings_path: Option<PathBuf>,
    settings: Mutex<SearchLogSettings>,
    write_lock: Mutex<()>,
    verbose_capture: Option<Arc<Mutex<Vec<u8>>>>,
    // Dropping this marker is what lets the last Toka process clean up the
    // session's JSON log without disturbing another open process.
    _session: Option<SessionMarker>,
}

impl SearchLogger {
    // Unconditional (not just `cfg(test)`): the app's tests build loggers too,
    // and this crate compiles without `cfg(test)` when they do.
    pub fn new(path: PathBuf) -> Self {
        Self::with_settings(path, None, SearchLogSettings::default(), None)
    }

    /// A logger that records nothing: no search log, no verbose logs. For
    /// callers that surface search errors themselves, like CLIs.
    pub fn disabled() -> Self {
        Self::new(PathBuf::new())
    }

    /// A logger that writes no files and instead captures every search's
    /// verbose trace in memory, for callers that stream it elsewhere (the
    /// `ts` CLI prints it to stderr). Drain with `take_verbose_trace`,
    /// usually through the search engine.
    pub fn with_verbose_capture() -> Self {
        Self {
            path: PathBuf::new(),
            settings_path: None,
            settings: Mutex::new(SearchLogSettings::default()),
            write_lock: Mutex::new(()),
            verbose_capture: Some(Arc::new(Mutex::new(Vec::new()))),
            _session: None,
        }
    }

    /// The verbose trace recorded since the last call. Empty unless this
    /// logger was built with `with_verbose_capture`.
    pub(crate) fn take_verbose_trace(&self) -> String {
        let Some(capture) = &self.verbose_capture else {
            return String::new();
        };
        let mut buffer = capture.lock().unwrap();
        let trace = String::from_utf8_lossy(&buffer).into_owned();
        buffer.clear();
        trace
    }

    fn system_path() -> PathBuf {
        #[cfg(target_os = "macos")]
        let path = dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Library/Logs")
            .join(crate::config::APP_LOG_DIR_NAME)
            .join("search.log");
        #[cfg(not(target_os = "macos"))]
        let path = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join(crate::config::APP_LOG_DIR_NAME)
            .join("logs/search.log");
        path
    }

    pub fn system() -> Self {
        let path = Self::system_path();
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
            verbose_capture: None,
            _session: session,
        }
    }

    pub fn settings(&self) -> SearchLogSettings {
        self.settings.lock().unwrap().clone()
    }

    pub fn set_settings(
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
        self.context_with_addon(request, with_verbose_log, None)
    }

    pub(crate) fn context_for_addon(
        &self,
        request: &SearchRequest,
        with_verbose_log: bool,
        addon_id: &str,
        addon_capture: &str,
    ) -> SearchLogContext {
        self.context_with_addon(request, with_verbose_log, Some((addon_id, addon_capture)))
    }

    fn context_with_addon(
        &self,
        request: &SearchRequest,
        with_verbose_log: bool,
        addon: Option<(&str, &str)>,
    ) -> SearchLogContext {
        let mut context = SearchLogContext::from(request);
        if let Some((addon_id, addon_capture)) = addon {
            context.addon_id = Some(addon_id.to_owned());
            context.addon_capture = Some(addon_capture.to_owned());
        }
        if let Some(capture) = &self.verbose_capture {
            // A capturing logger records every search, including match
            // counts, which never ask for a verbose log file.
            context.verbose = Some(Arc::new(VerboseSearchLog::capturing(
                capture.clone(),
                &context,
            )));
        } else if with_verbose_log {
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
    entry.insert("time".into(), json!(crate::timestamp::now_rfc3339()));
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
    entry.insert("sort".into(), json!(context.sort));
    if let Some(addon_id) = &context.addon_id {
        entry.insert("addonId".into(), json!(addon_id));
    }
    if let Some(addon_capture) = &context.addon_capture {
        entry.insert("addonCapture".into(), json!(addon_capture));
    }
    entry
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Every log file Toka writes: its log folder (the search log, its settings and
/// session markers, and on Linux the indexer log) and any verbose search logs
/// saved to the folder the user chose. The folder goes wholesale; the verbose
/// logs are file-only targets, so a directory at one of their paths refuses
/// instead of deleting.
pub fn reset_paths() -> Vec<crate::reset::ResetTarget> {
    use crate::reset::ResetTarget;
    let path = SearchLogger::system_path();
    let Some(directory) = path.parent() else {
        return Vec::new();
    };
    let settings = load_settings(&directory.join("search-log-settings.json"));
    let mut paths = vec![ResetTarget::dir(directory.to_path_buf())];
    if let Some(verbose) = settings.path {
        paths.extend(
            verbose_log_files(&verbose)
                .into_iter()
                .map(ResetTarget::file),
        );
    }
    paths
}

fn verbose_log_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files = entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(crate::config::VERBOSE_LOG_FILE_PREFIX)
                && name.ends_with(".log")
                && entry.file_type().is_ok_and(|kind| kind.is_file())
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    files.sort();
    files
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
        "{}{}-{}",
        crate::config::LOG_WRITE_PROBE_PREFIX,
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

struct VerboseSearchLog {
    sink: Mutex<Box<dyn Write + Send>>,
}

impl std::fmt::Debug for VerboseSearchLog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("VerboseSearchLog").finish()
    }
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
            "{}{}_{}_{}.log",
            crate::config::VERBOSE_LOG_FILE_PREFIX,
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
            sink: Mutex::new(Box::new(file)),
        }));
    }
    eprintln!(
        "Toka could not create a verbose search log in {}",
        directory.display()
    );
    None
}

fn write_verbose_header(sink: &mut dyn Write, context: &SearchLogContext) -> std::io::Result<()> {
    writeln!(sink, "Toka verbose search log")?;
    writeln!(sink, "Started: {}", crate::timestamp::now_rfc3339())?;
    writeln!(sink, "Query: {}", context.query)?;
    writeln!(sink, "Page: {}", context.page)?;
    writeln!(sink, "Page size: {}", context.page_size)?;
    writeln!(sink, "Media type: {:?}", context.media_type)?;
    writeln!(
        sink,
        "Sort: {:?} {:?}",
        context.sort.field, context.sort.direction
    )?;
    if let (Some(addon_id), Some(capture)) = (&context.addon_id, &context.addon_capture) {
        writeln!(sink, "Add-on: {addon_id} ({capture})")?;
    }
    writeln!(
        sink,
        "Fields: tags={}, file name={}, path={}",
        context.fields.tags, context.fields.file_name, context.fields.path
    )?;
    writeln!(sink)?;
    Ok(())
}

/// The shared bytes a capturing logger's searches append their trace to.
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl VerboseSearchLog {
    fn capturing(capture: Arc<Mutex<Vec<u8>>>, context: &SearchLogContext) -> Self {
        let log = Self {
            sink: Mutex::new(Box::new(CaptureWriter(capture)) as Box<dyn Write + Send>),
        };
        let _ = write_verbose_header(&mut *log.sink.lock().unwrap(), context);
        log
    }

    fn record_filtered(&self, path: &Path, reason: &str) {
        let mut sink = self.sink.lock().unwrap();
        let _ = writeln!(sink, "FILTERED OUT: {} ({reason})", path.display());
    }

    fn record_returned(&self, paths: &[PathBuf]) {
        let mut sink = self.sink.lock().unwrap();
        let _ = writeln!(sink, "RESULTS RETURNED TO APP:");
        if paths.is_empty() {
            let _ = writeln!(sink, "(none)");
        } else {
            for path in paths {
                let _ = writeln!(sink, "{}", path.display());
            }
        }
        let _ = writeln!(sink);
    }

    fn record_error(&self, error: &str) {
        let mut sink = self.sink.lock().unwrap();
        let _ = writeln!(sink, "SEARCH ERROR: {error}");
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

pub(crate) fn process_is_alive(pid: u32) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn verbose_log_header_has_a_readable_start_time() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("header.log");
        let mut file = fs::File::create(&path).unwrap();
        let context = SearchLogContext {
            query: "vacation".into(),
            page: 0,
            page_size: 10,
            fields: SearchFields::default(),
            media_type: MediaType::default(),
            sort: SearchSort::default(),
            addon_id: None,
            addon_capture: None,
            verbose: None,
        };
        write_verbose_header(&mut file, &context).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let started = text
            .lines()
            .find_map(|l| l.strip_prefix("Started: "))
            .unwrap();
        assert!(started.ends_with('Z') && started.contains('T'), "{started}");
    }

    #[test]
    fn verbose_log_files_are_only_toka_search_logs() {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "toka_search_log_1_2_3.log",
            "notes.log",
            "toka_search_log_x.txt",
        ] {
            fs::write(directory.path().join(name), b"x").unwrap();
        }
        fs::create_dir(directory.path().join("toka_search_log_dir.log")).unwrap();

        assert_eq!(
            verbose_log_files(directory.path()),
            vec![directory.path().join("toka_search_log_1_2_3.log")]
        );
    }

    #[test]
    fn verbose_logs_contain_filter_reasons_and_returned_paths() {
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
                search_id: None,
                fields: SearchFields::default(),
                media_type: MediaType::Videos,
                sort: SearchSort::default(),
            },
            true,
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
        assert!(body.contains("/Videos/summer.mp4"));
        assert!(body.contains("FILTERED OUT: /Videos/empty.mp4 (empty file)"));
        assert!(body.contains("RESULTS RETURNED TO APP:"));
    }

    #[test]
    fn verbose_capture_collects_the_trace_in_memory() {
        let logger = SearchLogger::with_verbose_capture();
        let context = logger.context(
            &SearchRequest {
                query: "summer".into(),
                page: 1,
                page_size: 24,
                search_id: None,
                fields: SearchFields::default(),
                media_type: MediaType::Videos,
                sort: SearchSort::default(),
            },
            true,
        );
        logger.record_filtered(&context, Path::new("/Videos/empty.mp4"), "empty file");
        logger.record_returned(&context, &[PathBuf::from("/Videos/summer.mp4")]);

        let trace = logger.take_verbose_trace();

        assert!(
            trace.contains("FILTERED OUT: /Videos/empty.mp4 (empty file)"),
            "{trace}"
        );
        assert!(trace.contains("RESULTS RETURNED TO APP:"), "{trace}");
        assert!(trace.contains("/Videos/summer.mp4"), "{trace}");
        assert!(logger.take_verbose_trace().is_empty());
    }

    #[test]
    fn verbose_capture_records_even_when_no_verbose_log_was_requested() {
        let logger = SearchLogger::with_verbose_capture();
        let context = logger.context(
            &SearchRequest {
                query: "summer".into(),
                page: 1,
                page_size: 24,
                search_id: None,
                fields: SearchFields::default(),
                media_type: MediaType::Videos,
                sort: SearchSort::default(),
            },
            false,
        );

        logger.record_filtered(&context, Path::new("/Videos/empty.mp4"), "empty file");

        assert!(logger
            .take_verbose_trace()
            .contains("FILTERED OUT: /Videos/empty.mp4 (empty file)"));
    }

    #[test]
    fn take_verbose_trace_is_empty_without_a_capture() {
        let logger = SearchLogger::new(PathBuf::new());
        let context = logger.context(
            &SearchRequest {
                query: "summer".into(),
                page: 1,
                page_size: 24,
                search_id: None,
                fields: SearchFields::default(),
                media_type: MediaType::Videos,
                sort: SearchSort::default(),
            },
            true,
        );

        logger.record_filtered(&context, Path::new("/Videos/empty.mp4"), "empty file");

        assert!(logger.take_verbose_trace().is_empty());
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
            sort: SearchSort::default(),
            addon_id: None,
            addon_capture: None,
            verbose: None,
        };

        logger.record_error(&context, "the search index is unavailable");

        let line = fs::read_to_string(directory.path().join("search.log")).unwrap();
        let entry: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(entry["kind"], "error");
        assert_eq!(entry["query"], "broken");
        assert_eq!(entry["error"], "the search index is unavailable");
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
