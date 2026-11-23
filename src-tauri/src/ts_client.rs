//! The app's only path to search and indexing: the `ts` shell utility.
//!
//! Ownership rule: `ts` alone opens the search index and supervises its
//! watchers. Nothing else in `src-tauri` may open the index read-write,
//! start watchers or supervisors, or read index state except through this
//! module. Reads `ts` serves go through `ts` too — search candidates,
//! folders, stats, thumbnails, syncs, resets, and service operations.
//! The sanctioned direct uses of `toka-search` from the app are this
//! client, pure helpers with no index IO (path utilities, JSON types,
//! the search session engine driven by [`TsSearchProvider`]), and the
//! fixture forwarding below.
//!
//! Every spawn resolves `ts` through [`resolve_ts`], carries a timeout
//! (no hangs: past the deadline the child is killed and the call
//! errors), and maps exits per the `ts` contract: 0 answers, 1 reports
//! the operation's own failure on stderr, 2 is a usage error that means
//! the app and `ts` disagree, and 3 means the daemon is not running.
//! `--json` documents must carry `"schema": 1`.

use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{Duration, Instant},
};
use toka_search::{
    index_stats::IndexStats,
    owned_index::{OwnedFolderStatus, OwnedIndexFolder, OwnedIndexState},
    providers::retain_visible_under_roots,
    search::{SearchError, SearchProvider},
    search_log::SearchLogContext,
};

/// How long a search may take before it is killed and reported: local
/// index reads answer in milliseconds; anything longer is stuck.
pub const TIMEOUT_SEARCH: Duration = Duration::from_secs(30);
/// Folder and stats reads answer as fast as searches.
pub const TIMEOUT_FOLDERS: Duration = Duration::from_secs(30);
pub const TIMEOUT_STATS: Duration = Duration::from_secs(30);
pub const TIMEOUT_STATUS: Duration = Duration::from_secs(15);
/// Syncs rewrite index rows and can wait on a busy database behind a
/// scan; they get longer than the reads.
pub const TIMEOUT_SYNC: Duration = Duration::from_secs(60);
/// Thumbnail and preview generation runs ffmpeg over the file; slow
/// media still finishes in minutes, never hangs forever.
pub const TIMEOUT_MEDIA: Duration = Duration::from_secs(300);
/// Service operations shell out to systemctl/launchctl and wait for the
/// daemon to start or stop.
pub const TIMEOUT_SERVICE: Duration = Duration::from_secs(180);
/// A reset stops the indexer before deleting; the stop can linger.
pub const TIMEOUT_RESET: Duration = Duration::from_secs(300);
/// The launch-page revision fallback: a best-effort read behind a command
/// thread, so a wedged `ts` stalls it for seconds, never the full stats
/// wait.
pub const TIMEOUT_REVISION: Duration = Duration::from_secs(5);

/// One finished `ts` run: its exit status with captured output. `status`
/// is -1 when the child never exited normally (a signal).
#[derive(Debug)]
pub struct TsOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Locates the `ts` binary every call in this module spawns: an explicit
/// `TS_BIN` override first (tests and development), then the `ts` beside
/// the running executable (development, e2e runs, and installs that ship
/// both binaries together), then `PATH`. No hardcoded absolute paths.
pub fn resolve_ts() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("TS_BIN") {
        let path = PathBuf::from(path);
        if !path.exists() {
            return Err(format!(
                "TS_BIN points at {}, which does not exist.",
                path.display()
            ));
        }
        if path.is_file() {
            return Ok(path);
        }
        return Err(format!(
            "TS_BIN points at {}, which is not a file.",
            path.display()
        ));
    }
    if let Ok(executable) = std::env::current_exe() {
        let sibling = sibling_ts(&executable);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    if let Some(path) = search_path("ts") {
        return Ok(path);
    }
    Err("Toka could not find the `ts` search helper: it is neither next to the Toka executable nor on PATH (set TS_BIN to its location).".to_owned())
}

/// The `ts` beside `executable`: where development, e2e, and installed
/// layouts keep both binaries together.
fn sibling_ts(executable: &Path) -> PathBuf {
    executable
        .parent()
        .map(|directory| directory.join("ts"))
        .unwrap_or_else(|| PathBuf::from("ts"))
}

fn search_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file() && is_runnable(candidate))
    })
}

/// Whether `candidate` can be spawned: on unix a regular file without any
/// execute bit is a shadow, not a binary, and the search must continue
/// past it instead of failing at spawn.
#[cfg(unix)]
fn is_runnable(candidate: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    candidate
        .metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_runnable(_candidate: &Path) -> bool {
    true
}

/// Runs `ts` with `args`, killing it past `timeout`. The exit status is
/// carried, not mapped — JSON callers parse through
/// [`parse_json_output`], and the `toka` CLI maps codes itself.
pub fn run(args: &[OsString], timeout: Duration) -> Result<TsOutput, String> {
    let binary = resolve_ts()?;
    let label = args
        .first()
        .map(|argument| argument.to_string_lossy().into_owned())
        .unwrap_or_else(|| "ts".to_owned());
    // `Command` panics on a NUL byte; a hostile or stray one must error
    // instead — never take the app down from a search box.
    if args.iter().any(|argument| has_nul(argument)) {
        return Err(format!(
            "ts {label} was passed an argument Toka cannot run."
        ));
    }
    let mut command = Command::new(&binary);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        // Its own process group, so the timeout below kills the whole
        // tree: service operations shell out to systemctl/launchctl and
        // thumbnails to ffmpeg, and those grandchildren must not outlive
        // the deadline invisibly.
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    forward_fixtures(&mut command);
    let mut child = command.spawn().map_err(|error| {
        format!(
            "Toka could not run its search helper ({}): {error}",
            binary.display()
        )
    })?;
    // Drain both pipes on reader threads while the main thread polls the
    // deadline: nothing may wait for the child's exit before reading, or a
    // child filling the 64 KiB pipe buffer (a broad `ts search --json`
    // answers the whole hit set) wedges forever and dies to a spurious
    // timeout.
    let stdout_reader = spawn_pipe_reader(child.stdout.take());
    let stderr_reader = spawn_pipe_reader(child.stderr.take());
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("Toka could not wait for its search helper: {error}"))?
        {
            return Ok(TsOutput {
                status: status.code().unwrap_or(-1),
                stdout: join_pipe_reader(stdout_reader)?,
                stderr: join_pipe_reader(stderr_reader)?,
            });
        }
        if Instant::now() >= deadline {
            kill_child_tree(&child);
            let _ = child.kill();
            let _ = child.wait();
            // The group kill reaps the grandchildren holding the pipes, so
            // these joins return instead of blocking on orphaned output.
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(format!(
                "ts {label} timed out after {} seconds.",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Reads one captured pipe to end on its own thread; see [`run`].
fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
) -> std::thread::JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut bytes)?;
        }
        Ok(bytes)
    })
}

/// Collects one [`spawn_pipe_reader`] thread: a failed read (or a reader
/// that never ran) errors exactly as the old `wait_with_output` did.
fn join_pipe_reader(
    reader: std::thread::JoinHandle<std::io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, String> {
    reader
        .join()
        .map_err(|_| "Toka could not read its search helper.".to_owned())?
        .map_err(|error| format!("Toka could not read its search helper: {error}"))
}

/// Kills the whole `ts` process group the child leads — grandchildren
/// included — ahead of the direct `kill` below.
#[cfg(unix)]
fn kill_child_tree(child: &std::process::Child) {
    // SAFETY: plain signal delivery to a process group this module created.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
}

/// No process groups off unix: the direct `kill` below is the whole tree.
#[cfg(not(unix))]
fn kill_child_tree(_child: &std::process::Child) {}

#[cfg(unix)]
fn has_nul(argument: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    argument.as_bytes().contains(&0)
}

#[cfg(not(unix))]
fn has_nul(argument: &OsStr) -> bool {
    argument.to_string_lossy().contains('\0')
}

/// Integration-test videos, forwarded to `ts` children whenever the app
/// itself was given them. Unconditional on purpose: production never
/// sets `TOKA_E2E_VIDEOS`, a `ts` without the fixtures feature ignores
/// `TS_FIXTURE_VIDEOS`, and the e2e app searches through `ts` too.
fn forward_fixtures(command: &mut Command) {
    if let Some(videos) = std::env::var_os("TOKA_E2E_VIDEOS") {
        command.env("TS_FIXTURE_VIDEOS", videos);
    }
}

/// Runs a `ts` argv from the `toka` CLI with the timeout its command
/// calls for.
pub fn run_cli(args: &[OsString]) -> Result<TsOutput, String> {
    let command = args
        .first()
        .and_then(|argument| argument.to_str())
        .unwrap_or("");
    run(args, timeout_for(command))
}

/// The timeout one `ts` command calls for, shared by the CLI runner and
/// the typed calls below.
pub(crate) fn timeout_for(command: &str) -> Duration {
    match command {
        "search" | "candidates" => TIMEOUT_SEARCH,
        "folders" => TIMEOUT_FOLDERS,
        "stats" => TIMEOUT_STATS,
        "status" => TIMEOUT_STATUS,
        "thumbnail" | "preview" => TIMEOUT_MEDIA,
        "sync-path" | "sync-rename" => TIMEOUT_SYNC,
        "reset" => TIMEOUT_RESET,
        _ => TIMEOUT_SERVICE,
    }
}

/// Maps a nonzero `ts` exit to its error, if it is one: exit 1 carries
/// stderr, exit 2 is a usage error — reachable only when the app and `ts`
/// disagree, such as an older `ts` that never learned the call — and exit
/// 3 means the daemon is not running. Exit 0 (and only it) is `None`.
fn nonzero_error(command: &str, output: &TsOutput) -> Option<String> {
    match output.status {
        0 => None,
        1 => Some(command_error(command, output)),
        2 => Some(disagreement_error(command, output)),
        3 => Some("The indexer is unavailable.".to_owned()),
        _ => Some(command_error(command, output)),
    }
}

/// The disagreement an exit-2 `ts <command>` reports: what `ts` said,
/// kept whole — with an older helper that never learned the call, its
/// usage text is the diagnosis.
fn disagreement_error(command: &str, output: &TsOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("Toka and its search helper disagree about `ts {command}`: the helper reported a usage error.")
    } else {
        format!("Toka and its search helper disagree about `ts {command}`: {stderr}")
    }
}

/// The failure `ts <command>` reported: its stderr without the `ts
/// <command>: ` prefix, so app errors read exactly as the in-process
/// ones they replace.
pub(crate) fn command_error(command: &str, output: &TsOutput) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let prefix = format!("ts {command}: ");
    let message = stderr.strip_prefix(prefix.as_str()).unwrap_or(stderr);
    if message.is_empty() {
        format!("ts {command} failed.")
    } else {
        message.to_owned()
    }
}

/// Parses one `ts <command> --json` answer: exit 0 carries a versioned
/// document, anything else is the error it reports. Schema 1 is
/// required; anything else (or malformed JSON) is a version mismatch
/// with a clear message, never a silent misread.
pub fn parse_json_output(command: &str, output: &TsOutput) -> Result<serde_json::Value, String> {
    if let Some(error) = nonzero_error(command, output) {
        return Err(error);
    }
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("`ts {command}` answered with unreadable JSON: {error}"))?;
    check_schema(command, &document)?;
    Ok(document)
}

fn check_schema(command: &str, document: &serde_json::Value) -> Result<(), String> {
    match document.get("schema") {
        Some(serde_json::Value::Number(schema)) if schema.as_u64() == Some(1) => Ok(()),
        _ => Err(format!(
            "`ts {command}` answered with an unknown schema: expected 1."
        )),
    }
}

/// Runs `ts` and parses its `--json` document in one go.
pub fn run_json(
    command: &str,
    args: &[OsString],
    timeout: Duration,
) -> Result<serde_json::Value, String> {
    let output = run(args, timeout)?;
    parse_json_output(command, &output)
}

fn field<'a>(
    document: &'a serde_json::Value,
    command: &str,
    name: &str,
) -> Result<&'a serde_json::Value, String> {
    document
        .get(name)
        .ok_or_else(|| format!("`ts {command}` answered without `{name}`."))
}

fn string_list(
    document: &serde_json::Value,
    command: &str,
    name: &str,
) -> Result<Vec<String>, String> {
    let entries = field(document, command, name)?;
    let entries = entries
        .as_array()
        .ok_or_else(|| format!("`ts {command}` answered with a non-list `{name}`."))?;
    entries
        .iter()
        .map(|entry| {
            entry
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("`ts {command}` answered with a non-text `{name}` entry."))
        })
        .collect()
}

/// Every path matching `query`, with the index revision the search saw:
/// the raw OR-widened set the engine then filters by fields, media type,
/// and add-ons exactly as it filtered provider candidates before. `ts
/// candidates` applies no field or NOT filtering of its own, so a
/// negation the viewer's fields would keep is never dropped inside `ts`.
pub fn search_candidates(query: &str) -> Result<(Vec<PathBuf>, u64), String> {
    let args = [
        OsString::from("candidates"),
        OsString::from("--json"),
        OsString::from("--"),
        OsString::from(query),
    ];
    let document = run_json("candidates", &args, TIMEOUT_SEARCH)?;
    let candidates = string_list(&document, "candidates", "candidates")?;
    let paths = candidates.into_iter().map(PathBuf::from).collect();
    let revision = field(&document, "candidates", "indexRevision")?
        .as_u64()
        .ok_or_else(|| "`ts candidates` answered without a numeric `indexRevision`.".to_owned())?;
    Ok((paths, revision))
}

/// Maps a `ts candidates` failure back to the search error it reports.
/// Only an empty query names a variant — the engine validates everything
/// else before spawning — so anything else is a provider failure
/// carrying `ts`'s own message, exactly as the owned-index provider did.
fn search_error_from_ts(message: String) -> SearchError {
    if message == SearchError::InvalidQuery.to_string() {
        SearchError::InvalidQuery
    } else {
        SearchError::Provider(message)
    }
}

/// A search provider that shells out to `ts`: candidates come from one
/// `ts candidates --json` per search, visibility rechecks against one
/// `ts folders list --json`, and the revision rides along with the
/// candidates. The engine on top keeps its snapshots, shuffling, result
/// ids, rename tracking, and search logging exactly as before — only the
/// index reads moved across the process boundary.
pub struct TsSearchProvider {
    revision: Mutex<Option<u64>>,
}

impl TsSearchProvider {
    pub fn new() -> Self {
        Self {
            revision: Mutex::new(None),
        }
    }
}

impl Default for TsSearchProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchProvider for TsSearchProvider {
    fn candidates(
        &self,
        query: &str,
        _whole_path: bool,
        _context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError> {
        // `ts candidates` always covers whole paths through the full-text
        // index, like the owned provider it replaces — there is no
        // narrower set to ask for, so the flag stays unused.
        match search_candidates(query) {
            Ok((paths, revision)) => {
                *self.revision.lock().unwrap() = Some(revision);
                Ok(paths)
            }
            Err(message) => Err(search_error_from_ts(message)),
        }
    }

    fn retain_visible_paths(&self, paths: &mut Vec<PathBuf>) -> Result<(), SearchError> {
        // Fixture runs serve exactly the configured videos: no watched
        // folders to scope to, so nothing is ever filtered — the old
        // fixture provider's no-op.
        if std::env::var_os("TOKA_E2E_VIDEOS").is_some() {
            return Ok(());
        }
        // Roots come from `ts`, the check itself is the provider's own:
        // `ts search` already filtered its answers, but renames the
        // engine recorded since still need the same visibility pass.
        let folders = folders_list().map_err(SearchError::Provider)?;
        let roots: Vec<PathBuf> = folders.into_iter().map(|folder| folder.path).collect();
        retain_visible_under_roots(&roots, paths);
        Ok(())
    }

    fn revision(&self) -> u64 {
        // The last search's revision: every engine call reads candidates
        // before the revision, so this is fresh. Only a launch page —
        // which never searches — falls back to a live read.
        if let Some(revision) = *self.revision.lock().unwrap() {
            return revision;
        }
        stats_with_timeout(TIMEOUT_REVISION)
            .map(|stats| stats.revision)
            .unwrap_or(0)
    }
}

/// The folders `ts` watches, with their indexing state.
pub fn folders_list() -> Result<Vec<OwnedIndexFolder>, String> {
    let args = [
        OsString::from("folders"),
        OsString::from("list"),
        OsString::from("--json"),
    ];
    let document = run_json("folders list", &args, TIMEOUT_FOLDERS)?;
    let folders = field(&document, "folders list", "folders")?;
    serde_json::from_value(folders.clone())
        .map_err(|error| format!("`ts folders list` answered with unreadable folders: {error}"))
}

/// Adds a folder exactly as the in-process manager did: `ts` validates,
/// canonicalizes, and overlaps-checks, and answers the resulting state.
pub fn folders_add(path: &Path) -> Result<OwnedIndexState, String> {
    let args = [
        OsString::from("folders"),
        OsString::from("add"),
        OsString::from("--json"),
        OsString::from("--"),
        path.as_os_str().to_os_string(),
    ];
    let document = run_json("folders add", &args, TIMEOUT_FOLDERS)?;
    index_state_from_document("folders add", &document)
}

/// Removes the folder `id` names, by id end to end: `ts folders remove`
/// resolves the id itself, so no list-then-remove race can slip between,
/// and folders whose paths are not valid UTF-8 remove like any other. An
/// id that resolves to nothing reports exactly what removing it in-process
/// did — the error, not a guess.
pub fn folders_remove_by_id(id: &str) -> Result<OwnedIndexState, String> {
    let args = [
        OsString::from("folders"),
        OsString::from("remove"),
        OsString::from("--json"),
        OsString::from("--"),
        OsString::from(id),
    ];
    match run_json("folders remove", &args, TIMEOUT_FOLDERS) {
        Ok(document) => index_state_from_document("folders remove", &document),
        // The folder left between the caller listing it and removing it:
        // `ts` names the id it cannot resolve, the app names the error the
        // in-process remove reported.
        Err(error) if error.contains("is not one of Toka's search folders") => {
            Err(toka_search::owned_index::OwnedIndexError::FolderNotConfigured.to_string())
        }
        Err(error) => Err(error),
    }
}

fn index_state_from_document(
    command: &str,
    document: &serde_json::Value,
) -> Result<OwnedIndexState, String> {
    serde_json::from_value(document.clone())
        .map_err(|error| format!("`ts {command}` answered with an unreadable index state: {error}"))
}

/// Everything the index knows, for `toka --index-stats`.
pub fn stats() -> Result<IndexStats, String> {
    stats_with_timeout(TIMEOUT_STATS)
}

/// One `ts stats --json` read under the caller's own deadline: the
/// revision fallback waits less than a deliberate stats read.
fn stats_with_timeout(timeout: Duration) -> Result<IndexStats, String> {
    let args = [OsString::from("stats"), OsString::from("--json")];
    stats_from_output(&run(&args, timeout)?)
}

fn stats_from_document(document: &serde_json::Value) -> Result<IndexStats, String> {
    serde_json::from_value(document.clone())
        .map_err(|error| format!("`ts stats` answered with unreadable stats: {error}"))
}

/// Parses one `ts stats --json` answer: the typed call above, split so
/// the `toka` CLI can run `ts` through its own injected runner.
pub fn stats_from_output(output: &TsOutput) -> Result<IndexStats, String> {
    stats_from_document(&parse_json_output("stats", output)?)
}

/// The full index state the setup screen polls: folders from `ts folders
/// list`, counts and revision from `ts stats`. The two reads split what
/// one in-process `state()` answered; a scan progressing between them
/// only skews one poll.
pub fn index_state() -> Result<OwnedIndexState, String> {
    let folders = folders_list()?;
    let stats = stats()?;
    let indexing_files = folders
        .iter()
        .filter(|folder| folder.status == OwnedFolderStatus::Indexing)
        .map(|folder| folder.scanned_files)
        .sum();
    Ok(OwnedIndexState {
        supported: true,
        revision: stats.revision,
        indexed_videos: stats.videos,
        indexed_images: stats.images,
        indexing_files,
        folders,
    })
}

/// The thumbnail for one file, generated on first run as the app did.
pub fn thumbnail(path: &Path) -> Result<PathBuf, String> {
    let args = [
        OsString::from("thumbnail"),
        OsString::from("--json"),
        OsString::from("--"),
        path.as_os_str().to_os_string(),
    ];
    let document = run_json("thumbnail", &args, TIMEOUT_MEDIA)?;
    let thumbnail = field(&document, "thumbnail", "thumbnail")?
        .as_str()
        .ok_or_else(|| "`ts thumbnail` answered without a `thumbnail` path.".to_owned())?;
    Ok(PathBuf::from(thumbnail))
}

/// The preview frames for one video, one path per frame.
pub fn preview(path: &Path) -> Result<Vec<PathBuf>, String> {
    let args = [
        OsString::from("preview"),
        OsString::from("--json"),
        OsString::from("--"),
        path.as_os_str().to_os_string(),
    ];
    let document = run_json("preview", &args, TIMEOUT_MEDIA)?;
    let frames = string_list(&document, "preview", "previews")?;
    Ok(frames.into_iter().map(PathBuf::from).collect())
}

/// Refreshes one changed path, as the app did after a rename, move, or
/// delete. A path under no watched folder is a race the in-process sync
/// silently absorbed — not an error worth reporting.
pub fn sync_path(path: &Path) -> Result<(), String> {
    let args = [
        OsString::from("sync-path"),
        OsString::from("--"),
        path.as_os_str().to_os_string(),
    ];
    sync_from_output(
        "sync-path",
        &run(&args, TIMEOUT_SYNC)?,
        "is not in any watched folder.",
    )
}

/// Moves one indexed path to its new name, as the app did when tagging
/// renamed a file or folder.
pub fn sync_rename(from: &Path, to: &Path) -> Result<(), String> {
    let args = [
        OsString::from("sync-rename"),
        OsString::from("--"),
        from.as_os_str().to_os_string(),
        to.as_os_str().to_os_string(),
    ];
    sync_from_output(
        "sync-rename",
        &run(&args, TIMEOUT_SYNC)?,
        "are not under the same watched folder.",
    )
}

fn sync_from_output(command: &str, output: &TsOutput, unmatched: &str) -> Result<(), String> {
    if output.status == 1 {
        let message = command_error(command, output);
        if message.contains(unmatched) {
            return Ok(());
        }
        return Err(message);
    }
    if let Some(error) = nonzero_error(command, output) {
        return Err(error);
    }
    Ok(())
}

/// Fire-and-forget syncs for the delete and tagging flows: failures are
/// logged exactly as the in-process syncs were, and never surface.
pub fn sync_path_best_effort(path: &Path) {
    if let Err(error) = sync_path(path) {
        eprintln!("Toka could not update its owned index: {error}");
    }
}

/// Fire-and-forget rename syncs; see [`sync_path_best_effort`].
pub fn sync_rename_best_effort(from: &Path, to: &Path) {
    if let Err(error) = sync_rename(from, to) {
        eprintln!("Toka could not update its owned index: {error}");
    }
}

/// The targets a `ts reset` preview lists.
#[derive(Debug)]
pub struct ResetPreview {
    pub targets: Vec<PathBuf>,
}

/// What a confirmed `ts reset` did: whether the pre-removal stop passed
/// (when it did not, nothing was removed), what left, and what failed.
#[derive(Debug)]
pub struct ResetApplied {
    pub indexer_stopped: bool,
    pub removed: Vec<PathBuf>,
    pub failures: Vec<String>,
}

/// Parses one `ts reset --json` preview, listed by the `toka` CLI
/// through its own injected runner.
pub fn reset_preview_from_output(output: &TsOutput) -> Result<ResetPreview, String> {
    let document = parse_json_output("reset", output)?;
    let targets = string_list(&document, "reset", "targets")?;
    Ok(ResetPreview {
        targets: targets.into_iter().map(PathBuf::from).collect(),
    })
}

/// Parses one `ts reset --yes --json` report, applied by the `toka` CLI
/// through its own injected runner. A failed reset still reports what
/// left before the failure, so the document parses on exit 1 as on exit
/// 0 — only a usage bug or an unreadable document errors here.
pub fn reset_applied_from_output(output: &TsOutput) -> Result<ResetApplied, String> {
    if output.status != 0 && output.status != 1 {
        if let Some(error) = nonzero_error("reset", output) {
            return Err(error);
        }
    }
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        // An exit-1 reset with no document is a planning error whose
        // reason `ts` printed on stderr: keep it, or the failure reads
        // as a parse bug instead of what `ts` refused.
        let mut message = format!("`ts reset` answered with unreadable JSON: {error}");
        if !String::from_utf8_lossy(&output.stderr).trim().is_empty() {
            message.push_str(&format!(" ({})", command_error("reset", output)));
        }
        message
    })?;
    check_schema("reset", &document)?;
    let indexer_stopped = field(&document, "reset", "indexerStopped")?
        .as_bool()
        .ok_or_else(|| "`ts reset` answered without a boolean `indexerStopped`.".to_owned())?;
    let removed = string_list(&document, "reset", "removed")?;
    let failures = string_list(&document, "reset", "failures")?;
    Ok(ResetApplied {
        indexer_stopped,
        removed: removed.into_iter().map(PathBuf::from).collect(),
        failures,
    })
}

/// One thing a `ts` service operation did — or found already done.
#[derive(Debug, Clone)]
pub struct ServiceAction {
    pub message: String,
    pub changed: bool,
}

/// What one `ts` service operation did: its actions in order, plus the
/// failure when the operation itself failed. A `Result::Err` here means
/// `ts` never answered — spawn, timeout, usage, or schema — not that the
/// operation failed.
#[derive(Debug, Clone)]
pub struct ServiceReport {
    pub actions: Vec<ServiceAction>,
    pub error: Option<String>,
}

/// Parses one `ts <service-command> --json` answer, success or partial
/// failure alike; split so `toka restart` can run `ts` through its own
/// host.
pub fn service_report_from_output(
    command: &str,
    output: &TsOutput,
) -> Result<ServiceReport, String> {
    // A failed operation still prints its partial actions as JSON; only
    // a usage bug, an unavailable daemon, or an unreadable document is a
    // client error.
    if output.status != 0 && output.status != 1 {
        if let Some(error) = nonzero_error(command, output) {
            return Err(error);
        }
    }
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("`ts {command}` answered with unreadable JSON: {error}"))?;
    check_schema(command, &document)?;
    let actions = field(&document, command, "actions")?;
    let actions = actions
        .as_array()
        .ok_or_else(|| format!("`ts {command}` answered with a non-list `actions`."))?;
    let mut lines = Vec::with_capacity(actions.len());
    for action in actions {
        let message = action
            .get("message")
            .and_then(|message| message.as_str())
            .ok_or_else(|| format!("`ts {command}` answered an action without `message`."))?;
        let changed = action
            .get("changed")
            .and_then(|changed| changed.as_bool())
            .ok_or_else(|| format!("`ts {command}` answered an action without `changed`."))?;
        lines.push(ServiceAction {
            message: message.to_owned(),
            changed,
        });
    }
    let error = document
        .get("error")
        .and_then(|error| error.as_str())
        .map(str::to_owned);
    Ok(ServiceReport {
        actions: lines,
        error,
    })
}

/// Installs the indexer service idempotently: what the app ensures at
/// startup so the first launch after upgrade works with no user action.
/// A service that is already installed and running is left alone —
/// opening a worktree build must not repoint the user's service at the
/// worktree copy, and upgrades converge through the installer's explicit
/// `ts install` plus restart. Any probe failure installs exactly as
/// before (fail-open).
pub fn ensure_installed() -> Result<ServiceReport, String> {
    if service_is_healthy() {
        return Ok(ServiceReport {
            actions: vec![ServiceAction {
                message: "indexer service already installed and running".to_owned(),
                changed: false,
            }],
            error: None,
        });
    }
    let args = [OsString::from("install"), OsString::from("--json")];
    service_report_from_output("install", &run(&args, TIMEOUT_SERVICE)?)
}

/// Whether `ts status --json` reports an installed, running service.
/// The document parses on any exit status (a live service whose daemon
/// has not heartbeated yet still exits 3); anything unreadable is not
/// healthy, and the caller installs.
fn service_is_healthy() -> bool {
    let args = [OsString::from("status"), OsString::from("--json")];
    let Ok(output) = run(&args, TIMEOUT_STATUS) else {
        return false;
    };
    let Ok(document) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return false;
    };
    if check_schema("status", &document).is_err() {
        return false;
    }
    document.get("serviceInstalled") == Some(&serde_json::Value::Bool(true))
        && document.get("serviceRunning") == Some(&serde_json::Value::Bool(true))
}

/// Serializes the tests that mutate the process environment (`TS_BIN`,
/// `PATH`, `TOKA_E2E_VIDEOS`, `XDG_CACHE_HOME`, `HOME`, `TMPDIR`):
/// parallel tests share one environment, so each holds the lock while it
/// swaps variables in and back out. One lock for the whole crate:
/// `ts_client`, `cli`, and `lib` tests all spawn or resolve through the
/// environment.
#[cfg(test)]
pub(crate) mod test_env {
    use std::ffi::{OsStr, OsString};
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    pub struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvGuard {
        pub fn lock() -> Self {
            Self {
                _lock: ENV_LOCK.lock().unwrap(),
                saved: Vec::new(),
            }
        }

        pub fn set(&mut self, key: &'static str, value: &OsStr) {
            if !self.saved.iter().any(|(saved, _)| *saved == key) {
                self.saved.push((key, std::env::var_os(key)));
            }
            std::env::set_var(key, value);
        }

        pub fn remove(&mut self, key: &'static str) {
            if !self.saved.iter().any(|(saved, _)| *saved == key) {
                self.saved.push((key, std::env::var_os(key)));
            }
            std::env::remove_var(key);
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_env::EnvGuard;
    use super::*;

    /// A fake `ts`: asserts its argv, then answers the canned bodies.
    #[cfg(unix)]
    fn write_shim(directory: &Path, name: &str, script: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Points `TS_BIN` at `shim` with a hermetic test environment. Hold
    /// the guard for the whole test: dropping it restores the variables.
    #[cfg(unix)]
    fn use_ts(shim: &Path) -> EnvGuard {
        let mut env = EnvGuard::lock();
        env.set("TS_BIN", shim.as_os_str());
        env.remove("TOKA_E2E_VIDEOS");
        env
    }

    fn output(status: i32, stdout: &str, stderr: &str) -> TsOutput {
        TsOutput {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn timeouts_give_searches_seconds_and_freshness_minutes() {
        assert_eq!(timeout_for("search"), TIMEOUT_SEARCH);
        assert_eq!(timeout_for("candidates"), TIMEOUT_SEARCH);
        assert_eq!(timeout_for("folders"), TIMEOUT_FOLDERS);
        assert_eq!(timeout_for("stats"), TIMEOUT_STATS);
        assert_eq!(timeout_for("status"), TIMEOUT_STATUS);
        assert_eq!(timeout_for("thumbnail"), TIMEOUT_MEDIA);
        assert_eq!(timeout_for("preview"), TIMEOUT_MEDIA);
        assert_eq!(timeout_for("sync-path"), TIMEOUT_SYNC);
        assert_eq!(timeout_for("sync-rename"), TIMEOUT_SYNC);
        assert_eq!(timeout_for("reset"), TIMEOUT_RESET);
        assert_eq!(timeout_for("restart"), TIMEOUT_SERVICE);
        assert_eq!(timeout_for("install"), TIMEOUT_SERVICE);
        assert_eq!(timeout_for("bogus"), TIMEOUT_SERVICE);
        assert!(TIMEOUT_SEARCH < TIMEOUT_SYNC);
        assert!(TIMEOUT_SYNC < TIMEOUT_MEDIA);
        assert!(TIMEOUT_REVISION < TIMEOUT_STATS);
    }

    #[test]
    fn sibling_ts_sits_beside_the_executable() {
        assert_eq!(
            sibling_ts(Path::new("/opt/toka/Toka")),
            PathBuf::from("/opt/toka/ts")
        );
        assert_eq!(sibling_ts(Path::new("toka")), PathBuf::from("ts"));
    }

    #[cfg(unix)]
    #[test]
    fn ts_bin_override_wins_over_everything() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let shim = write_shim(&bin, "ts", "#!/bin/sh\necho shim\n");
        let _env = use_ts(&shim);

        assert_eq!(resolve_ts().unwrap(), shim);
    }

    #[cfg(unix)]
    #[test]
    fn ts_bin_pointing_nowhere_errors_clearly() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("no-ts-here");
        let mut env = EnvGuard::lock();
        env.set("TS_BIN", missing.as_os_str());

        let error = resolve_ts().unwrap_err();

        assert!(error.contains("TS_BIN"), "{error}");
        assert!(error.contains("does not exist"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn ts_bin_pointing_at_a_directory_errors_clearly() {
        let directory = tempfile::tempdir().unwrap();
        let mut env = EnvGuard::lock();
        env.set("TS_BIN", directory.path().as_os_str());

        let error = resolve_ts().unwrap_err();

        assert!(error.contains("TS_BIN"), "{error}");
        assert!(error.contains("is not a file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_finds_ts_when_no_override_or_sibling_applies() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let shim = write_shim(&bin, "ts", "#!/bin/sh\necho shim\n");
        let mut env = EnvGuard::lock();
        env.remove("TS_BIN");
        env.set("PATH", bin.as_os_str());

        assert_eq!(resolve_ts().unwrap(), shim);
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_skips_a_non_executable_shadow() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let shadow = directory.path().join("shadow");
        let bin = directory.path().join("bin");
        std::fs::create_dir(&shadow).unwrap();
        std::fs::create_dir(&bin).unwrap();
        let decoy = shadow.join("ts");
        std::fs::write(&decoy, b"not a binary\n").unwrap();
        std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o644)).unwrap();
        let shim = write_shim(&bin, "ts", "#!/bin/sh\necho shim\n");
        let mut env = EnvGuard::lock();
        env.remove("TS_BIN");
        let path = std::env::join_paths([&shadow, &bin]).unwrap();
        env.set("PATH", path.as_os_str());

        assert_eq!(resolve_ts().unwrap(), shim);
    }

    #[cfg(unix)]
    #[test]
    fn resolution_without_any_ts_says_where_it_looked() {
        let directory = tempfile::tempdir().unwrap();
        let empty = directory.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        let mut env = EnvGuard::lock();
        env.remove("TS_BIN");
        env.set("PATH", empty.as_os_str());

        let error = resolve_ts().unwrap_err();

        assert!(error.contains("PATH"), "{error}");
        assert!(error.contains("TS_BIN"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn run_carries_status_and_output_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            "#!/bin/sh\necho 'out line'\necho 'err line' >&2\nexit 5\n",
        );
        let _env = use_ts(&shim);

        let output = run(&[OsString::from("search")], Duration::from_secs(10)).unwrap();

        assert_eq!(output.status, 5);
        assert_eq!(output.stdout, b"out line\n");
        assert_eq!(output.stderr, b"err line\n");
    }

    #[cfg(unix)]
    #[test]
    fn run_kills_a_hung_child_past_its_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(directory.path(), "ts", "#!/bin/sh\nexec sleep 30\n");
        let _env = use_ts(&shim);

        let started = Instant::now();
        let error = run(&[OsString::from("search")], Duration::from_millis(100)).unwrap_err();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(error.contains("timed out"), "{error}");
        assert!(error.contains("ts search"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn run_returns_large_stdout_without_deadlocking() {
        let directory = tempfile::tempdir().unwrap();
        // 2 MiB: far past the 64 KiB pipe buffer, so a `run()` that only
        // reads after the child exits would wedge the child until the
        // deadline and report a spurious timeout.
        let shim = write_shim(
            directory.path(),
            "ts",
            "#!/bin/sh\nhead -c 2000000 /dev/zero | tr '\\0' 'o'\n",
        );
        let _env = use_ts(&shim);

        let started = Instant::now();
        let output = run(&[OsString::from("search")], Duration::from_secs(10)).unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(output.status, 0);
        assert_eq!(output.stdout.len(), 2_000_000);
        assert!(output.stdout.iter().all(|byte| *byte == b'o'));
        assert!(output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn run_returns_large_stderr_without_deadlocking() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            "#!/bin/sh\nhead -c 2000000 /dev/zero | tr '\\0' 'e' >&2\n",
        );
        let _env = use_ts(&shim);

        let started = Instant::now();
        let output = run(&[OsString::from("search")], Duration::from_secs(10)).unwrap();

        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(output.status, 0);
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr.len(), 2_000_000);
        assert!(output.stderr.iter().all(|byte| *byte == b'e'));
    }

    #[cfg(unix)]
    #[test]
    fn run_refuses_nul_arguments_instead_of_panicking() {
        use std::os::unix::ffi::OsStringExt;
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(directory.path(), "ts", "#!/bin/sh\nexit 9\n");
        let _env = use_ts(&shim);

        let error = run(
            &[
                OsString::from("search"),
                OsString::from_vec(b"a\0b".to_vec()),
            ],
            Duration::from_secs(10),
        )
        .unwrap_err();

        assert!(error.contains("cannot run"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn fixture_videos_reach_ts_children() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            "#!/bin/sh\necho \"fixtures=$TS_FIXTURE_VIDEOS\"\n",
        );
        let mut env = EnvGuard::lock();
        env.set("TS_BIN", shim.as_os_str());
        env.set("TOKA_E2E_VIDEOS", OsStr::new("/videos/a.mp4:/videos/b.mp4"));

        let output = run(&[OsString::from("search")], Duration::from_secs(10)).unwrap();

        assert_eq!(output.stdout, b"fixtures=/videos/a.mp4:/videos/b.mp4\n");
    }

    #[cfg(unix)]
    #[test]
    fn no_fixture_env_without_fixture_videos() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            "#!/bin/sh\necho \"fixtures=${TS_FIXTURE_VIDEOS-unset}\"\n",
        );
        let _env = use_ts(&shim);

        let output = run(&[OsString::from("search")], Duration::from_secs(10)).unwrap();

        assert_eq!(output.stdout, b"fixtures=unset\n");
    }

    #[test]
    fn versioned_documents_parse_and_anything_else_errors() {
        let document =
            parse_json_output("search", &output(0, r#"{"schema": 1, "results": []}"#, "")).unwrap();
        assert_eq!(document["results"], serde_json::json!([]));

        for (stdout, message) in [
            (r#"{"schema": 2, "results": []}"#, "unknown schema"),
            (r#"{"results": []}"#, "unknown schema"),
            (r#"{"schema": "1"}"#, "unknown schema"),
            (r#"[]"#, "unknown schema"),
            (r#"{"schema": 1"#, "unreadable JSON"),
            (r#"not json"#, "unreadable JSON"),
        ] {
            let error = parse_json_output("search", &output(0, stdout, "")).unwrap_err();
            assert!(error.contains(message), "{stdout}: {error}");
            assert!(error.contains("ts search"), "{stdout}: {error}");
        }
    }

    #[test]
    fn exit_1_strips_the_command_prefix_and_exit_3_means_unavailable() {
        let error = parse_json_output(
            "folders add",
            &output(1, "", "ts folders add: That folder is already covered.\n"),
        )
        .unwrap_err();
        assert_eq!(error, "That folder is already covered.");

        // Without the prefix (a shim, a future ts), the message stands.
        let error = parse_json_output("stats", &output(1, "", "boom\n")).unwrap_err();
        assert_eq!(error, "boom");

        let error = parse_json_output("stats", &output(1, "", "")).unwrap_err();
        assert_eq!(error, "ts stats failed.");

        let error = parse_json_output("status", &output(3, "{}", "")).unwrap_err();
        assert_eq!(error, "The indexer is unavailable.");

        let error = parse_json_output("search", &output(7, "", "ts search: gone\n")).unwrap_err();
        assert_eq!(error, "gone");
    }

    #[test]
    fn exit_2_reports_an_app_and_ts_disagreement_in_ts_own_words() {
        // An older `ts` that never learned the subcommand answers usage,
        // not JSON: the error names the disagreement and keeps what `ts`
        // said, instead of blaming the app for misusing its helper.
        let error = parse_json_output(
            "candidates",
            &output(
                2,
                "",
                "ts: unknown command `candidates`\nUsage: ts <command>\n",
            ),
        )
        .unwrap_err();
        assert!(error.contains("disagree"), "{error}");
        assert!(error.contains("unknown command"), "{error}");
        assert!(!error.contains("misused"), "{error}");

        let error = parse_json_output("stats", &output(2, "", "")).unwrap_err();
        assert!(error.contains("disagree"), "{error}");
        assert!(error.contains("ts stats"), "{error}");
    }

    #[test]
    fn stats_documents_parse_and_malformed_ones_error() {
        let stats = stats_from_output(&output(
            0,
            r#"{"schema": 1, "revision": 9, "videos": 3, "images": 2, "serviceRunning": true, "folders": []}"#,
            "",
        ))
        .unwrap();
        assert_eq!(stats.revision, 9);
        assert_eq!(stats.videos, 3);
        assert_eq!(stats.images, 2);

        let error = stats_from_output(&output(0, r#"{"schema": 1}"#, "")).unwrap_err();
        assert!(error.contains("unreadable stats"), "{error}");

        let error = stats_from_output(&output(1, "", "ts stats: no index\n")).unwrap_err();
        assert_eq!(error, "no index");
    }

    #[test]
    fn reset_documents_parse_with_their_stop_flag() {
        let preview =
            reset_preview_from_output(&output(0, r#"{"schema": 1, "targets": ["/a", "/b"]}"#, ""))
                .unwrap();
        assert_eq!(
            preview.targets,
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );

        // A failed reset still parses: what left before the failure is
        // part of the report `toka reset` renders.
        let applied = reset_applied_from_output(&output(
            1,
            r#"{"schema": 1, "indexerStopped": false, "removed": [], "failures": ["nope"]}"#,
            "ts reset: nope\n",
        ))
        .unwrap();
        assert!(!applied.indexer_stopped);
        assert!(applied.removed.is_empty());
        assert_eq!(applied.failures, vec!["nope".to_owned()]);

        let applied = reset_applied_from_output(&output(
            0,
            r#"{"schema": 1, "indexerStopped": true, "removed": ["/a"], "failures": []}"#,
            "",
        ))
        .unwrap();
        assert!(applied.indexer_stopped);
        assert_eq!(applied.removed, vec![PathBuf::from("/a")]);
        assert!(applied.failures.is_empty());
    }

    #[test]
    fn reset_without_a_document_keeps_ts_stderr_reason() {
        // An exit-1 reset that printed no JSON is a planning error: the
        // error keeps `ts`'s own reason instead of only naming the parse.
        let error = reset_applied_from_output(&output(
            1,
            "",
            "ts reset: refusing to remove /state/index.sqlite3: a directory occupies this file's path\n",
        ))
        .unwrap_err();
        assert!(error.contains("unreadable JSON"), "{error}");
        assert!(
            error.contains(
                "refusing to remove /state/index.sqlite3: a directory occupies this file's path"
            ),
            "{error}"
        );

        // Nothing on stderr either: the parse error stands alone.
        let error = reset_applied_from_output(&output(1, "", "")).unwrap_err();
        assert!(error.contains("unreadable JSON"), "{error}");
        assert!(!error.contains('('), "{error}");

        // A valid exit-1 report still decodes to its stop flag and lists.
        let applied = reset_applied_from_output(&output(
            1,
            r#"{"schema": 1, "indexerStopped": true, "removed": ["/a"], "failures": ["nope"]}"#,
            "ts reset: nope\n",
        ))
        .unwrap();
        assert!(applied.indexer_stopped);
        assert_eq!(applied.removed, vec![PathBuf::from("/a")]);
        assert_eq!(applied.failures, vec!["nope".to_owned()]);
    }

    #[test]
    fn service_reports_carry_actions_and_partial_failures() {
        let report = service_report_from_output(
            "restart",
            &output(
                0,
                r#"{"schema": 1, "command": "restart", "actions": [{"message": "started", "changed": true}]}"#,
                "",
            ),
        )
        .unwrap();
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].message, "started");
        assert!(report.actions[0].changed);
        assert_eq!(report.error, None);

        let report = service_report_from_output(
            "restart",
            &output(
                1,
                r#"{"schema": 1, "command": "restart", "actions": [{"message": "stopped", "changed": true}], "error": "not installed"}"#,
                "ts restart: not installed\n",
            ),
        )
        .unwrap();
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].message, "stopped");
        assert!(report.actions[0].changed);
        assert_eq!(report.error.as_deref(), Some("not installed"));

        let error = service_report_from_output("restart", &output(1, "garbage", "ts restart: x\n"))
            .unwrap_err();
        assert!(error.contains("unreadable JSON"), "{error}");

        let error = service_report_from_output(
            "restart",
            &output(0, r#"{"schema": 1, "command": "restart"}"#, ""),
        )
        .unwrap_err();
        assert!(error.contains("`actions`"), "{error}");
    }

    /// A folders document with one ready and one indexing folder.
    #[cfg(unix)]
    fn folders_document() -> String {
        r#"{"schema": 1, "folders": [
            {"id": "aaa", "path": "/media/movies", "status": "ready", "message": null, "scannedFiles": 10, "currentPath": null, "recentDirectories": []},
            {"id": "bbb", "path": "/media/new", "status": "indexing", "message": null, "scannedFiles": 5, "currentPath": "/media/new/part", "recentDirectories": []}
        ]}"#
        .to_owned()
    }

    #[cfg(unix)]
    #[test]
    fn folders_list_parses_the_watched_folders() {
        let directory = tempfile::tempdir().unwrap();
        let document = folders_document();
        let shim = write_shim(
            directory.path(),
            "ts",
            &format!(
                "#!/bin/sh\nif [ \"$1\" != folders ] || [ \"$2\" != list ] || [ \"$3\" != --json ]; then echo \"unexpected argv: $*\" >&2; exit 9; fi\ncat <<'EOF'\n{document}\nEOF\n"
            ),
        );
        let _env = use_ts(&shim);

        let folders = folders_list().unwrap();

        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].id, "aaa");
        assert_eq!(folders[0].path, PathBuf::from("/media/movies"));
        assert_eq!(folders[1].status, OwnedFolderStatus::Indexing);
        assert_eq!(folders[1].scanned_files, 5);
    }

    #[cfg(unix)]
    #[test]
    fn folders_add_sends_the_path_and_returns_the_state() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" != folders ] || [ "$2" != add ] || [ "$3" != --json ] || [ "$4" != -- ] || [ "$5" != "/media/movies" ]; then echo "unexpected argv: $*" >&2; exit 9; fi
cat <<'EOF'
{"schema": 1, "supported": true, "revision": 4, "indexedVideos": 1, "indexedImages": 0, "indexingFiles": 0, "folders": [
    {"id": "aaa", "path": "/media/movies", "status": "pending", "message": null, "scannedFiles": 0, "currentPath": null, "recentDirectories": []}
]}
EOF
"#,
        );
        let _env = use_ts(&shim);

        let state = folders_add(Path::new("/media/movies")).unwrap();

        assert!(state.supported);
        assert_eq!(state.revision, 4);
        assert_eq!(state.folders.len(), 1);
        assert_eq!(state.folders[0].path, PathBuf::from("/media/movies"));
    }

    #[cfg(unix)]
    #[test]
    fn folders_add_validation_errors_read_as_before() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            "#!/bin/sh\necho 'ts folders add: Choose a folder to search.' >&2\nexit 1\n",
        );
        let _env = use_ts(&shim);

        let error = folders_add(Path::new("/media/file.mp4")).unwrap_err();

        assert_eq!(error, "Choose a folder to search.");
    }

    #[cfg(unix)]
    #[test]
    fn folders_remove_passes_the_id_straight_through() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" = folders ] && [ "$2" = remove ] && [ "$3" = --json ] && [ "$4" = -- ] && [ "$5" = bbb ]; then cat <<'EOF'
{"schema": 1, "supported": true, "revision": 5, "indexedVideos": 0, "indexedImages": 0, "indexingFiles": 0, "folders": []}
EOF
else echo "unexpected argv: $*" >&2; exit 9; fi
"#,
        );
        let _env = use_ts(&shim);

        // No `folders list` runs first: the id goes straight to `ts`, so
        // no list-then-remove race can slip between.
        let state = folders_remove_by_id("bbb").unwrap();

        assert_eq!(state.revision, 5);
        assert!(state.folders.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn folders_remove_of_an_unknown_id_reports_the_configured_error() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" = folders ] && [ "$2" = remove ] && [ "$5" = zzz ]; then echo "ts folders remove: zzz is not one of Toka's search folders." >&2; exit 1
else echo "unexpected argv: $*" >&2; exit 9; fi
"#,
        );
        let _env = use_ts(&shim);

        let error = folders_remove_by_id("zzz").unwrap_err();

        assert_eq!(
            error,
            toka_search::owned_index::OwnedIndexError::FolderNotConfigured.to_string()
        );
    }

    #[cfg(unix)]
    #[test]
    fn index_state_merges_folders_with_stats_counts() {
        let directory = tempfile::tempdir().unwrap();
        let folders = folders_document();
        let shim = write_shim(
            directory.path(),
            "ts",
            &format!(
                "#!/bin/sh\nif [ \"$1\" = folders ]; then cat <<'EOF'\n{folders}\nEOF\nelif [ \"$1\" = stats ]; then cat <<'EOF'\n{{\"schema\": 1, \"revision\": 9, \"videos\": 3, \"images\": 2, \"serviceRunning\": true, \"folders\": []}}\nEOF\nelse echo \"unexpected argv: $*\" >&2; exit 9; fi\n"
            ),
        );
        let _env = use_ts(&shim);

        let state = index_state().unwrap();

        assert!(state.supported);
        assert_eq!(state.revision, 9);
        assert_eq!(state.indexed_videos, 3);
        assert_eq!(state.indexed_images, 2);
        assert_eq!(state.indexing_files, 5);
        assert_eq!(state.folders.len(), 2);
    }

    #[test]
    fn search_failures_map_back_to_their_variants() {
        assert!(matches!(
            search_error_from_ts("Enter at least one search term.".to_owned()),
            SearchError::InvalidQuery
        ));
        match search_error_from_ts("Toka could not read its search index.".to_owned()) {
            SearchError::Provider(message) => {
                assert_eq!(message, "Toka could not read its search index.");
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    /// A `ts` answering raw candidates and the folders behind them:
    /// the candidates call asserts its argv (flags before `--`, the query
    /// verbatim after it).
    #[cfg(unix)]
    fn candidates_shim(directory: &Path, hits: &str, revision: u64) -> PathBuf {
        let folders = folders_document();
        write_shim(
            directory,
            "ts",
            &format!(
                "#!/bin/sh\nif [ \"$1\" = candidates ]; then if [ \"$2\" != --json ] || [ \"$3\" != -- ] || [ -z \"$4\" ]; then echo \"unexpected candidates argv: $*\" >&2; exit 9; fi\ncat <<'EOF'\n{{\"schema\": 1, \"query\": \"clip\", \"indexRevision\": {revision}, \"candidates\": [{hits}]}}\nEOF\nelif [ \"$1\" = folders ]; then cat <<'EOF'\n{folders}\nEOF\nelse echo \"unexpected argv: $*\" >&2; exit 9; fi\n"
            ),
        )
    }

    #[cfg(unix)]
    fn search_request(query: &str) -> toka_search::search::SearchRequest {
        use toka_search::search::{MediaType, SearchFields, SearchRequest, PAGE_SIZE};
        SearchRequest {
            query: query.to_owned(),
            page: 1,
            page_size: PAGE_SIZE,
            search_id: None,
            fields: SearchFields::default(),
            media_type: MediaType::Videos,
        }
    }

    #[cfg(unix)]
    #[test]
    fn engine_search_filters_ts_candidates_like_provider_ones() {
        use std::sync::Arc;
        use toka_search::search::SearchEngine;
        let directory = tempfile::tempdir().unwrap();
        let shim = candidates_shim(
            directory.path(),
            r#""/media/movies/clip.mp4", "/media/movies/.hidden-clip.mp4", "/elsewhere/clip-elsewhere.mp4""#,
            7,
        );
        let _env = use_ts(&shim);
        // Roots are /media/movies and /media/new: the hidden file and
        // the outsider fall out of the visibility recheck, and only the
        // visible hit still matches.
        let engine = SearchEngine::new(Arc::new(TsSearchProvider::new()));

        let page = engine.search(search_request("clip")).unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "clip.mp4");
        assert_eq!(page.index_revision, 7);
        assert_eq!(engine.match_count(search_request("clip")).unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn provider_backed_engine_matches_the_direct_engine_over_one_index() {
        use std::sync::Arc;
        use toka_search::{
            owned_index::{OwnedIndex, OwnedIndexManager},
            providers::OwnedIndexSearchProvider,
            search::{MediaType, SearchEngine, SearchFields, SearchRequest, PAGE_SIZE},
            search_log::SearchLogContext,
        };

        // One real index both engines read: the direct engine through its
        // in-process provider, the `ts`-backed engine through a shim whose
        // candidates answers are derived from that same provider — exactly
        // what `ts candidates` serves (proven wire-identical by the `ts`
        // integration test). A negation the viewer's fields would keep
        // must survive in both.
        let directory = tempfile::tempdir().unwrap();
        let media = directory.path().join("media");
        std::fs::create_dir_all(media.join("archive")).unwrap();
        for name in [
            "archive/clip.mp4",
            "clip.mp4",
            "talk [beach].mp4",
            "beach-day.mp4",
        ] {
            std::fs::write(media.join(name), b"fixture video").unwrap();
        }
        let root = media.canonicalize().unwrap();
        let database = directory.path().join("state").join("index.sqlite3");
        let manager = OwnedIndexManager::under(database.clone());
        let state = manager.add_folder(&root).unwrap();
        OwnedIndex::open(&database)
            .unwrap()
            .scan_root(&state.folders[0].id, &root, |_| {})
            .unwrap();
        let direct_provider = OwnedIndexSearchProvider::new(database);
        let revision = direct_provider.revision();

        let folders = serde_json::json!({
            "schema": 1,
            "folders": serde_json::to_value(manager.folders().unwrap()).unwrap(),
        });
        let queries = ["clip NOT archive", "NOT beach", "beach", "archive", "clip"];
        let mut cases = String::new();
        for query in queries {
            let request = SearchRequest {
                query: query.to_owned(),
                page: 1,
                page_size: PAGE_SIZE,
                search_id: None,
                fields: SearchFields::default(),
                media_type: MediaType::Videos,
            };
            let context = SearchLogContext::from(&request);
            let derived = direct_provider.candidates(query, true, &context).unwrap();
            let document = serde_json::json!({
                "schema": 1,
                "query": query,
                "indexRevision": revision,
                "candidates": derived
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            });
            cases.push_str(&format!(
                "    \"{query}\") cat <<'EOF'\n{document}\nEOF\n      ;;\n"
            ));
        }
        let shim = write_shim(
            directory.path(),
            "ts",
            &format!(
                "#!/bin/sh\nif [ \"$1\" = candidates ]; then if [ \"$2\" != --json ] || [ \"$3\" != -- ]; then echo \"unexpected candidates argv: $*\" >&2; exit 9; fi\n  case \"$4\" in\n{cases}    *) echo \"unexpected query: $4\" >&2; exit 9;;\n  esac\nelif [ \"$1\" = folders ]; then cat <<'EOF'\n{folders}\nEOF\nelse echo \"unexpected argv: $*\" >&2; exit 9; fi\n"
            ),
        );
        let _env = use_ts(&shim);

        let wired = SearchEngine::new(Arc::new(TsSearchProvider::new()));
        let direct = SearchEngine::new(Arc::new(OwnedIndexSearchProvider::new(
            directory.path().join("state").join("index.sqlite3"),
        )));
        let names = |page: &toka_search::search::SearchPage| {
            let mut names: Vec<String> = page
                .results
                .iter()
                .map(|result| result.file_name.clone())
                .collect();
            names.sort();
            names
        };
        // Both bots' NOT examples plus positive field-restriction cases:
        // `clip NOT archive` with Path disabled keeps the archived clip
        // (the path-only term cannot exclude under these fields), and
        // filename-only `NOT beach` keeps the tag-only match.
        let all_fields = SearchFields::default();
        for (query, fields, expected) in [
            (
                "clip NOT archive",
                SearchFields {
                    tags: true,
                    file_name: true,
                    path: false,
                },
                vec!["clip.mp4", "clip.mp4"],
            ),
            (
                "NOT beach",
                SearchFields {
                    tags: false,
                    file_name: true,
                    path: false,
                },
                vec!["clip.mp4", "clip.mp4", "talk [beach].mp4"],
            ),
            (
                "beach",
                SearchFields {
                    tags: false,
                    file_name: true,
                    path: false,
                },
                vec!["beach-day.mp4"],
            ),
            (
                "archive",
                SearchFields {
                    tags: false,
                    file_name: false,
                    path: true,
                },
                vec!["clip.mp4"],
            ),
            ("clip", all_fields, vec!["clip.mp4", "clip.mp4"]),
        ] {
            let request = SearchRequest {
                query: query.to_owned(),
                page: 1,
                page_size: PAGE_SIZE,
                search_id: None,
                fields,
                media_type: MediaType::Videos,
            };
            let wired_page = wired.search(request.clone()).unwrap();
            let direct_page = direct.search(request.clone()).unwrap();
            assert_eq!(
                names(&wired_page),
                names(&direct_page),
                "query: {query} ({fields:?})"
            );
            // The expected hits pin the semantics, not just the parity:
            // with the old full-search wire format the first two cases
            // lost a hit inside `ts`.
            let mut expected: Vec<String> = expected.into_iter().map(str::to_owned).collect();
            expected.sort();
            assert_eq!(names(&wired_page), expected, "query: {query}");
            assert_eq!(
                wired.match_count(request.clone()).unwrap(),
                direct.match_count(request.clone()).unwrap(),
                "match count: {query}"
            );
            assert_eq!(
                wired.match_count(request.clone()).unwrap(),
                expected.len(),
                "match count: {query}"
            );
            assert_eq!(wired_page.index_revision, direct_page.index_revision);
        }
    }

    #[cfg(unix)]
    #[test]
    fn fixture_runs_keep_every_candidate_without_watched_folders() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" != folders ] || [ "$2" != list ] || [ "$3" != --json ]; then echo "unexpected argv: $*" >&2; exit 9; fi
cat <<'EOF'
{"schema": 1, "folders": []}
EOF
"#,
        );
        let mut env = EnvGuard::lock();
        env.set("TS_BIN", shim.as_os_str());
        env.set("TOKA_E2E_VIDEOS", OsStr::new("/videos/a.mp4:/videos/b.mp4"));
        let provider = TsSearchProvider::new();
        let mut paths = vec![
            PathBuf::from("/videos/a.mp4"),
            PathBuf::from("/videos/b.mp4"),
        ];

        provider.retain_visible_paths(&mut paths).unwrap();

        assert_eq!(
            paths,
            vec![
                PathBuf::from("/videos/a.mp4"),
                PathBuf::from("/videos/b.mp4")
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn visibility_still_drops_paths_outside_watched_folders() {
        let directory = tempfile::tempdir().unwrap();
        let document = folders_document();
        let shim = write_shim(
            directory.path(),
            "ts",
            &format!(
                "#!/bin/sh\nif [ \"$1\" != folders ] || [ \"$2\" != list ] || [ \"$3\" != --json ]; then echo \"unexpected argv: $*\" >&2; exit 9; fi\ncat <<'EOF'\n{document}\nEOF\n"
            ),
        );
        let _env = use_ts(&shim);
        let provider = TsSearchProvider::new();
        let mut paths = vec![
            PathBuf::from("/media/movies/clip.mp4"),
            PathBuf::from("/elsewhere/clip.mp4"),
        ];

        provider.retain_visible_paths(&mut paths).unwrap();

        assert_eq!(paths, vec![PathBuf::from("/media/movies/clip.mp4")]);
    }

    #[cfg(unix)]
    #[test]
    fn dash_queries_reach_ts_verbatim_after_the_separator() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" != candidates ] || [ "$2" != --json ] || [ "$3" != -- ] || [ "$4" != "--dash" ]; then echo "unexpected argv: $*" >&2; exit 9; fi
cat <<'EOF'
{"schema": 1, "query": "--dash", "indexRevision": 1, "candidates": []}
EOF
"#,
        );
        let _env = use_ts(&shim);

        let (paths, revision) = search_candidates("--dash").unwrap();

        assert!(paths.is_empty());
        assert_eq!(revision, 1);
    }

    #[cfg(unix)]
    #[test]
    fn ts_candidates_failures_surface_as_provider_errors() {
        use std::sync::Arc;
        use toka_search::search::SearchEngine;
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            "#!/bin/sh\necho 'ts candidates: Toka could not read its search index.' >&2\nexit 1\n",
        );
        let _env = use_ts(&shim);
        let engine = SearchEngine::new(Arc::new(TsSearchProvider::new()));

        let error = engine.search(search_request("clip")).unwrap_err();

        match error {
            SearchError::Provider(message) => {
                assert_eq!(message, "Toka could not read its search index.");
            }
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn empty_queries_fail_before_any_spawn() {
        use std::sync::Arc;
        use toka_search::search::SearchEngine;
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            "#!/bin/sh\necho 'must not spawn' >&2\nexit 9\n",
        );
        let _env = use_ts(&shim);
        let engine = SearchEngine::new(Arc::new(TsSearchProvider::new()));

        let error = engine.search(search_request("   ")).unwrap_err();

        assert!(matches!(error, SearchError::InvalidQuery));
    }

    #[cfg(unix)]
    #[test]
    fn revision_comes_from_the_last_search_without_a_stats_call() {
        use std::sync::Arc;
        use toka_search::search::SearchEngine;
        let directory = tempfile::tempdir().unwrap();
        let shim = candidates_shim(directory.path(), r#""/media/movies/clip.mp4""#, 7);
        let _env = use_ts(&shim);
        let provider = Arc::new(TsSearchProvider::new());
        let engine = SearchEngine::new(provider.clone());
        engine.search(search_request("clip")).unwrap();

        // The shim answers no `stats`: a call there would exit 9 and fail.
        assert_eq!(provider.revision(), 7);
    }

    #[cfg(unix)]
    #[test]
    fn revision_without_a_search_falls_back_to_a_live_read() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" != stats ] || [ "$2" != --json ]; then echo "unexpected argv: $*" >&2; exit 9; fi
cat <<'EOF'
{"schema": 1, "revision": 11, "videos": 0, "images": 0, "serviceRunning": null, "folders": []}
EOF
"#,
        );
        let _env = use_ts(&shim);

        assert_eq!(TsSearchProvider::new().revision(), 11);
    }

    #[cfg(unix)]
    #[test]
    fn thumbnail_and_preview_return_ts_paths() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" = thumbnail ] && [ "$2" = --json ] && [ "$3" = -- ] && [ "$4" = "/media/clip.mp4" ]; then cat <<'EOF'
{"schema": 1, "thumbnail": "/tmp/thumb.jpg"}
EOF
elif [ "$1" = preview ] && [ "$2" = --json ] && [ "$3" = -- ] && [ "$4" = "/media/clip.mp4" ]; then cat <<'EOF'
{"schema": 1, "previews": ["/tmp/f1.jpg", "/tmp/f2.jpg"]}
EOF
else echo "unexpected argv: $*" >&2; exit 9; fi
"#,
        );
        let _env = use_ts(&shim);

        assert_eq!(
            thumbnail(Path::new("/media/clip.mp4")).unwrap(),
            PathBuf::from("/tmp/thumb.jpg")
        );
        assert_eq!(
            preview(Path::new("/media/clip.mp4")).unwrap(),
            vec![PathBuf::from("/tmp/f1.jpg"), PathBuf::from("/tmp/f2.jpg")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn syncs_succeed_silently_and_swallow_unmatched_races() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" = sync-path ] && [ "$2" = -- ] && [ "$3" = "/media/gone.mp4" ]; then echo "ts sync-path: /media/gone.mp4 is not in any watched folder." >&2; exit 1
elif [ "$1" = sync-path ] && [ "$2" = -- ]; then exit 0
elif [ "$1" = sync-rename ] && [ "$2" = -- ] && [ "$3" = "/media/a.mp4" ]; then echo "ts sync-rename: /media/a.mp4 and /media/b.mp4 are not under the same watched folder." >&2; exit 1
elif [ "$1" = sync-rename ] && [ "$2" = -- ]; then exit 0
else echo "unexpected argv: $*" >&2; exit 9; fi
"#,
        );
        let _env = use_ts(&shim);

        sync_path(Path::new("/media/clip.mp4")).unwrap();
        sync_path(Path::new("/media/gone.mp4")).unwrap();
        sync_rename(Path::new("/media/a.mp4"), Path::new("/media/b.mp4")).unwrap();
        sync_rename(Path::new("/media/c.mp4"), Path::new("/media/d.mp4")).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sync_hard_errors_surface() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            "#!/bin/sh\necho 'ts sync-path: Toka could not use its search index: busy.' >&2\nexit 1\n",
        );
        let _env = use_ts(&shim);

        let error = sync_path(Path::new("/media/clip.mp4")).unwrap_err();

        assert_eq!(error, "Toka could not use its search index: busy.");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_installed_skips_install_when_the_service_is_healthy() {
        // A healthy service is left alone: the probe reports installed
        // and running, so `install` must never run (the shim fails it).
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" = status ] && [ "$2" = --json ]; then cat <<'EOF'
{"schema": 1, "running": true, "serviceInstalled": true, "serviceRunning": true}
EOF
else echo "install must not run: $*" >&2; exit 9; fi
"#,
        );
        let _env = use_ts(&shim);

        let report = ensure_installed().unwrap();

        assert_eq!(report.error, None);
        assert_eq!(report.actions.len(), 1);
        assert!(!report.actions[0].changed);
        assert!(
            report.actions[0]
                .message
                .contains("already installed and running"),
            "{}",
            report.actions[0].message
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_installed_proceeds_unless_the_service_is_healthy() {
        // Anything but installed-and-running installs exactly as before:
        // a stopped or missing service, unknown fields, a failed probe,
        // or an unreadable document all fail open to `install`.
        for (name, status_body) in [
            (
                "stopped",
                r#"{"schema": 1, "running": false, "serviceInstalled": true, "serviceRunning": false}"#,
            ),
            (
                "missing",
                r#"{"schema": 1, "running": false, "serviceInstalled": false, "serviceRunning": null}"#,
            ),
            (
                "unknown",
                r#"{"schema": 1, "running": false, "serviceInstalled": null, "serviceRunning": null}"#,
            ),
            ("unreadable", "not json"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let shim = write_shim(
                directory.path(),
                "ts",
                &format!(
                    "#!/bin/sh\nif [ \"$1\" = status ]; then cat <<'EOF'\n{status_body}\nEOF\nelif [ \"$1\" = install ] && [ \"$2\" = --json ]; then cat <<'EOF'\n{{\"schema\": 1, \"command\": \"install\", \"actions\": [{{\"message\": \"installed for {name}\", \"changed\": true}}]}}\nEOF\nelse echo \"unexpected argv: $*\" >&2; exit 9; fi\n"
                ),
            );
            let _env = use_ts(&shim);

            let report = ensure_installed().unwrap();

            assert_eq!(report.actions.len(), 1, "{name}: the install must have run");
            assert_eq!(report.actions[0].message, format!("installed for {name}"));
            assert!(report.actions[0].changed);
        }

        // A probe that fails outright (no document at all) installs too.
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" = status ]; then echo "ts status: no service manager" >&2; exit 1
elif [ "$1" = install ]; then cat <<'EOF'
{"schema": 1, "command": "install", "actions": [{"message": "installed after probe failure", "changed": true}]}
EOF
else echo "unexpected argv: $*" >&2; exit 9; fi
"#,
        );
        let _env = use_ts(&shim);

        let report = ensure_installed().unwrap();

        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].message, "installed after probe failure");
    }

    #[cfg(unix)]
    #[test]
    fn ensure_installed_asks_for_json_and_reports_actions() {
        let directory = tempfile::tempdir().unwrap();
        let shim = write_shim(
            directory.path(),
            "ts",
            r#"#!/bin/sh
if [ "$1" != install ] || [ "$2" != --json ] || [ $# != 2 ]; then echo "unexpected argv: $*" >&2; exit 9; fi
cat <<'EOF'
{"schema": 1, "command": "install", "actions": [{"message": "already installed", "changed": false}]}
EOF
"#,
        );
        let _env = use_ts(&shim);

        let report = ensure_installed().unwrap();
        assert_eq!(report.actions.len(), 1);
        assert_eq!(report.actions[0].message, "already installed");
        assert!(!report.actions[0].changed);
        assert_eq!(report.error, None);
    }
}
