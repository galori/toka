use serde::Serialize;
use std::{
    ffi::{OsStr, OsString},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use toka_search::{
    daemon_state::{self, DaemonActivity, DaemonStatus},
    index_stats::{self, IndexStats},
    owned_index::{OwnedFolderStatus, OwnedIndexFolder, OwnedIndexState},
    search::{MediaType, SearchEngine, SearchFields, SearchRequest, VideoResult, PAGE_SIZE},
};

/// The version of every `--json` document this CLI prints. The app checks it
/// before parsing.
const JSON_SCHEMA_VERSION: u32 = 1;

const GENERAL_HELP: &str = "Usage: ts <command> [arguments]\n\nCommands:\n  search <query>   Search indexed media and print file paths\n  stats            Show what the indexer has found\n  status           Show whether the indexer daemon is running\n  folders ...      Add, remove, or list the folders the index watches\n  sync-path <path> Refresh one changed path in the index\n  sync-rename <from> <to>   Move one indexed path to its new name\n  thumbnail <path> Show or generate one file's thumbnail\n  preview <path>   Show or generate one video's preview frames\n\nOptions:\n  -h, --help      Show this help\n  -v, --version   Show the ts version and Git SHA";
const SEARCH_USAGE: &str = "Usage: ts search <query> [--json] [--limit N] [--page N] [--type video|image|all] [--count] [--verbose]";

const STATS_USAGE: &str = "Usage: ts stats [--json]";
const STATS_HELP: &str = "Usage: ts stats [--json]\n\nShow whether the indexer is active, the number of videos and images indexed, and each search folder's status and file counts. Changes nothing in the index.";
const STATUS_USAGE: &str = "Usage: ts status [--json] [--watch]";
const STATUS_HELP: &str = "Usage: ts status [--json] [--watch]\n\nShow whether the indexer daemon is running and what it is doing: its process, uptime, current activity (idle, scanning, repairing, thumbnails, offline-waiting, or unknown from a newer daemon), the folder and file under the cursor, and the work still queued. Folders whose storage is missing count as offline and are listed on their own line, never as queued work; folders backing off after an error still count as pending until they scan. Exits 3 when the daemon is not running, so scripts can branch on it. Changes nothing in the index.\n\n  --json       Print one JSON document (schema 1) instead of the report\n  --watch      Re-render every second until the daemon is idle with nothing queued; offline folders never hold the wait. Stop it with Ctrl-C";
const DAEMON_HELP: &str =
    "Usage: ts daemon <command>\n\nCommands:\n  run   Run the indexer daemon in the foreground";
const DAEMON_RUN_USAGE: &str = "Usage: ts daemon run";
const DAEMON_RUN_HELP: &str = "Usage: ts daemon run\n\nRun the indexer daemon in the foreground until stopped: watching every search folder, generating thumbnails, and publishing a heartbeat for `ts status`. This is the service manager's entry point, not an interactive command. One supervisor owns each index: whichever starts first wins, so a second `ts daemon run` on the same index exits 1 instead of starting. The Toka app takes the same lock, so running a daemon alongside it is safe: the app yields its own watchers with a warning and keeps reading the index.";
const FOLDERS_HELP: &str = "Usage: ts folders <command>\n\nCommands:\n  add <path> [--json]      Add a folder to the index\n  remove <path> [--json]   Remove a folder from the index\n  list [--json]            List the folders the index watches";
const FOLDERS_LIST_USAGE: &str = "Usage: ts folders list [--json]";
const FOLDERS_LIST_HELP: &str = "Usage: ts folders list [--json]\n\nList the folders the index watches, one per line with its indexing state. Changes nothing in the index.";
const FOLDERS_ADD_USAGE: &str = "Usage: ts folders add <path> [--json]";
const FOLDERS_ADD_HELP: &str = "Usage: ts folders add <path> [--json]\n\nAdd a folder to the index, exactly as adding it in the app does. The running indexer picks it up and starts scanning on its own; on macOS with the app quit, the folder waits for the next launch. Prints a confirmation, or one JSON document (schema 1) of the resulting folder state with --json.\n\nUse -- to add a path starting with a dash.";
const FOLDERS_REMOVE_USAGE: &str = "Usage: ts folders remove <path> [--json]";
const FOLDERS_REMOVE_HELP: &str = "Usage: ts folders remove <path> [--json]\n\nRemove a folder from the index, exactly as removing it in the app does: its files leave the index and the running indexer stops watching it. Prints a confirmation, or one JSON document (schema 1) of the resulting folder state with --json.\n\nUse -- to remove a path starting with a dash.";
const SYNC_PATH_USAGE: &str = "Usage: ts sync-path <path>";
const SYNC_PATH_HELP: &str = "Usage: ts sync-path <path>\n\nRefresh one path in the index after it changed on disk, as the app does after a rename, move, or delete. The path must sit under a watched folder; anything else is an error. Prints nothing. A missing index is left alone, exactly as in the app.\n\nUse -- to sync a path starting with a dash.";
const SYNC_RENAME_USAGE: &str = "Usage: ts sync-rename <from> <to>";
const SYNC_RENAME_HELP: &str = "Usage: ts sync-rename <from> <to>\n\nMove one indexed path to its new name after a rename, as the app does when tagging renames a file or folder. Both paths must sit under the same watched folder; anything else is an error and the index is left untouched. Renaming a watched folder itself re-points its roots row to the new path. Moving a watched folder into another watched folder still succeeds and nests the two roots, exactly as in the app. Prints nothing. A missing index is left alone, exactly as in the app.\n\nUse -- to rename a path starting with a dash.";
const THUMBNAIL_USAGE: &str = "Usage: ts thumbnail <path> [--json]";
const THUMBNAIL_HELP: &str = "Usage: ts thumbnail <path> [--json]\n\nPrint the thumbnail for one file, generating it on the first run as the app does; an image prints as its own thumbnail. Prints the thumbnail path, or one JSON document (schema 1) with --json.\n\nUse -- for a path starting with a dash.";
const PREVIEW_USAGE: &str = "Usage: ts preview <path> [--json]";
const PREVIEW_HELP: &str = "Usage: ts preview <path> [--json]\n\nPrint the preview frames for one video, one per line, generating them on the first run as the app does on hover; an image prints as its own single frame. Prints one JSON document (schema 1) with --json.\n\nUse -- for a path starting with a dash.";

fn search_help() -> String {
    format!(
        "{SEARCH_USAGE}\n\nSearch Toka's indexed videos and images. Print one full path per result line, sorted by path so pages stay stable across runs.\n\n  --json       Print one JSON document (schema 1) instead of paths\n  --limit N    Print at most N results (applied after --page)\n  --page N     Print page N of the results ({PAGE_SIZE} per page)\n  --type T     Only video, image, or all results (default all)\n  --count      Print only how many results matched\n  --verbose    Stream the per-search trace to stderr\n\nUse -- to search for text starting with a dash."
    )
}

fn build_version() -> &'static str {
    pick_version(option_env!("TS_BUILD_VERSION"))
}

fn pick_version(build_version: Option<&'static str>) -> &'static str {
    build_version
        .filter(|version| !version.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// The version line also shows the same Git SHA as the Toka launch screen.
/// The build hands it over as `TOKA_GIT_SHA`; a build with no provenance has
/// none, so it reads "development" exactly as the frontend does.
fn version_text(version: &str, git_sha: Option<&str>) -> String {
    let sha = git_sha
        .filter(|sha| !sha.is_empty())
        .unwrap_or("development");
    format!("ts {version} (Git SHA {sha})")
}

/// Every constructor a command can call, injected so the unit tests stub
/// them out and no test touches the real index. One parameter per
/// constructor reads better than a bundle here: each closure's distinct
/// type already makes mixing two up a compile error.
#[allow(clippy::too_many_arguments)]
pub fn run_command(
    arguments: &[OsString],
    create_engine: impl FnOnce() -> Result<SearchEngine, String>,
    create_verbose_engine: impl FnOnce() -> Result<SearchEngine, String>,
    read_index_stats: impl FnOnce() -> Result<IndexStats, String>,
    read_folders: impl FnOnce() -> Result<Vec<OwnedIndexFolder>, String>,
    add_folder: impl FnOnce(&Path) -> Result<(OwnedIndexState, PathBuf), String>,
    remove_folder: impl FnOnce(&str) -> Result<OwnedIndexState, String>,
    sync_path: impl FnOnce(&Path) -> Result<(), String>,
    sync_rename: impl FnOnce(&Path, &Path) -> Result<(), String>,
    make_thumbnail: impl FnOnce(&Path) -> Result<PathBuf, String>,
    make_preview: impl FnOnce(&Path) -> Result<Vec<PathBuf>, String>,
    read_status: impl FnMut() -> Result<DaemonStatus, String>,
    run_daemon: impl FnOnce() -> Result<(), String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let Some(command) = arguments.get(1) else {
        let _ = writeln!(stderr, "{GENERAL_HELP}");
        return 2;
    };

    if command == OsStr::new("--version") || command == OsStr::new("-v") {
        return print_text(
            &version_text(build_version(), option_env!("TOKA_GIT_SHA")),
            stdout,
            stderr,
        );
    }

    if command == OsStr::new("--help") || command == OsStr::new("-h") {
        return print_text(GENERAL_HELP, stdout, stderr);
    }

    if command == OsStr::new("search") {
        return run_search_command(
            &arguments[2..],
            create_engine,
            create_verbose_engine,
            stdout,
            stderr,
        );
    }

    if command == OsStr::new("stats") {
        return run_stats_command(&arguments[2..], read_index_stats, stdout, stderr);
    }

    if command == OsStr::new("folders") {
        return run_folders_command(
            &arguments[2..],
            read_folders,
            add_folder,
            remove_folder,
            stdout,
            stderr,
        );
    }

    if command == OsStr::new("sync-path") {
        return run_sync_path_command(&arguments[2..], sync_path, stdout, stderr);
    }

    if command == OsStr::new("sync-rename") {
        return run_sync_rename_command(&arguments[2..], sync_rename, stdout, stderr);
    }

    if command == OsStr::new("thumbnail") {
        return run_thumbnail_command(&arguments[2..], make_thumbnail, stdout, stderr);
    }

    if command == OsStr::new("preview") {
        return run_preview_command(&arguments[2..], make_preview, stdout, stderr);
    }

    if command == OsStr::new("status") {
        return run_status_command(&arguments[2..], read_status, stdout, stderr);
    }

    // Hidden on purpose: `daemon run` is the service manager's entry
    // point, not an interactive command, so it stays out of the help.
    if command == OsStr::new("daemon") {
        return run_daemon_command(&arguments[2..], run_daemon, stdout, stderr);
    }

    let _ = writeln!(stderr, "{GENERAL_HELP}");
    2
}

fn run_folders_command(
    arguments: &[OsString],
    read_folders: impl FnOnce() -> Result<Vec<OwnedIndexFolder>, String>,
    add_folder: impl FnOnce(&Path) -> Result<(OwnedIndexState, PathBuf), String>,
    remove_folder: impl FnOnce(&str) -> Result<OwnedIndexState, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(FOLDERS_HELP, stdout, stderr);
    }
    let Some((subcommand, rest)) = arguments.split_first() else {
        let _ = writeln!(stderr, "{FOLDERS_HELP}");
        return 2;
    };
    if subcommand == OsStr::new("list") {
        return run_folders_list_command(rest, read_folders, stdout, stderr);
    }
    if subcommand == OsStr::new("add") {
        return run_folders_add_command(rest, add_folder, stdout, stderr);
    }
    if subcommand == OsStr::new("remove") {
        return run_folders_remove_command(rest, read_folders, remove_folder, stdout, stderr);
    }
    let _ = writeln!(stderr, "{FOLDERS_HELP}");
    2
}

fn run_folders_list_command(
    arguments: &[OsString],
    read_folders: impl FnOnce() -> Result<Vec<OwnedIndexFolder>, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(FOLDERS_LIST_HELP, stdout, stderr);
    }
    let Some(json) = parse_json_flag(arguments) else {
        let _ = writeln!(stderr, "{FOLDERS_LIST_USAGE}");
        return 2;
    };

    let folders = match read_folders() {
        Ok(folders) => folders,
        Err(error) => {
            let _ = writeln!(stderr, "ts folders list: {error}");
            return 1;
        }
    };
    if json {
        // Encode fallibly like stats does: a future non-lossy field must
        // exit 1, never panic inside `json!`.
        let folders = match serde_json::to_value(&folders) {
            Ok(folders) => folders,
            Err(error) => {
                let _ = writeln!(stderr, "ts folders list: could not encode results: {error}");
                return 1;
            }
        };
        let document = serde_json::json!({
            "schema": JSON_SCHEMA_VERSION,
            "folders": folders,
        });
        return print_json(&document, "ts folders list", stdout, stderr);
    }
    for folder in folders {
        let mut line = format!(
            "{} [{}]",
            folder.path.display(),
            folder_status_label(folder.status)
        );
        if let Some(message) = &folder.message {
            line.push_str(&format!(" - {message}"));
        }
        if let Err(error) = writeln!(stdout, "{line}") {
            let _ = writeln!(stderr, "ts folders list: could not write a folder: {error}");
            return 1;
        }
    }
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "ts folders list: could not write folders: {error}");
        return 1;
    }
    0
}

fn run_folders_add_command(
    arguments: &[OsString],
    add_folder: impl FnOnce(&Path) -> Result<(OwnedIndexState, PathBuf), String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(FOLDERS_ADD_HELP, stdout, stderr);
    }
    let (paths, json) = match parse_path_arguments(arguments, true) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(stderr, "{message}\n{FOLDERS_ADD_USAGE}");
            return 2;
        }
    };
    if paths.is_empty() {
        let _ = writeln!(
            stderr,
            "Folders add needs a folder path.\n{FOLDERS_ADD_USAGE}"
        );
        return 2;
    }
    if paths.len() > 1 {
        let _ = writeln!(
            stderr,
            "Folders add takes one folder path.\n{FOLDERS_ADD_USAGE}"
        );
        return 2;
    }

    // The manager resolves once and reports what it committed; echoing
    // that spelling — never a resolution of our own — keeps a symlink
    // retargeted mid-add from printing a path the index does not contain.
    let (state, committed) = match add_folder(&paths[0]) {
        Ok(added) => added,
        Err(error) => {
            let _ = writeln!(stderr, "ts folders add: {error}");
            return 1;
        }
    };
    if json {
        return print_index_state(&state, "ts folders add", stdout, stderr);
    }
    match writeln!(stdout, "Added {}", committed.display()).and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "ts folders add: could not write: {error}");
            1
        }
    }
}

/// The folder state after an add or a remove, as one versioned document:
/// what the app's own add/remove flows return, plus the schema version.
fn print_index_state(
    state: &OwnedIndexState,
    command: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    // Encode fallibly like stats does: a future non-lossy field must
    // exit 1, never panic inside `json!`.
    let mut document = match serde_json::to_value(state) {
        Ok(document) => document,
        Err(error) => {
            let _ = writeln!(stderr, "{command}: could not encode results: {error}");
            return 1;
        }
    };
    document["schema"] = serde_json::json!(JSON_SCHEMA_VERSION);
    print_json(&document, command, stdout, stderr)
}

fn run_folders_remove_command(
    arguments: &[OsString],
    read_folders: impl FnOnce() -> Result<Vec<OwnedIndexFolder>, String>,
    remove_folder: impl FnOnce(&str) -> Result<OwnedIndexState, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(FOLDERS_REMOVE_HELP, stdout, stderr);
    }
    let (paths, json) = match parse_path_arguments(arguments, true) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(stderr, "{message}\n{FOLDERS_REMOVE_USAGE}");
            return 2;
        }
    };
    if paths.is_empty() {
        let _ = writeln!(
            stderr,
            "Folders remove needs a folder path.\n{FOLDERS_REMOVE_USAGE}"
        );
        return 2;
    }
    if paths.len() > 1 {
        let _ = writeln!(
            stderr,
            "Folders remove takes one folder path.\n{FOLDERS_REMOVE_USAGE}"
        );
        return 2;
    }

    let folders = match read_folders() {
        Ok(folders) => folders,
        Err(error) => {
            let _ = writeln!(stderr, "ts folders remove: {error}");
            return 1;
        }
    };
    // The app removes by id; the shell names a path, so resolve it to the
    // configured folder first. Unlike the app's remove flow — which opens
    // (and thereby creates) the database — a missing index errors here
    // instead: `read_folders` reports no folders when the database file is
    // absent, so nothing matches and the remove below never runs.
    let Some(folder) = resolve_folder(&folders, &paths[0]) else {
        let _ = writeln!(
            stderr,
            "ts folders remove: {} is not one of Toka's search folders.",
            paths[0].display()
        );
        return 1;
    };
    let state = match remove_folder(&folder.id) {
        Ok(state) => state,
        Err(error) => {
            let _ = writeln!(stderr, "ts folders remove: {error}");
            return 1;
        }
    };
    if json {
        return print_index_state(&state, "ts folders remove", stdout, stderr);
    }
    match writeln!(stdout, "Removed {}", folder.path.display()).and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "ts folders remove: could not write: {error}");
            1
        }
    }
}

/// The configured folder `requested` names. The normalized spelling
/// matches first: recorded entries are canonical absolute paths, and the
/// normalization also resolves a typed spelling against the current
/// directory, so a folder deleted from disk — often the very reason to
/// remove it — still matches what the same shell spelling added. The
/// spelling as typed only matches when nothing normalized does, so one
/// request can never ambiguously name two rows.
fn resolve_folder<'a>(
    folders: &'a [OwnedIndexFolder],
    requested: &Path,
) -> Option<&'a OwnedIndexFolder> {
    let spelling = toka_search::owned_index::canonical_spelling(requested);
    folders
        .iter()
        .find(|folder| folder.path == spelling)
        .or_else(|| folders.iter().find(|folder| folder.path == requested))
}

/// The human label for a folder state, kept identical to its JSON value (see
/// the folder status label test).
fn folder_status_label(status: OwnedFolderStatus) -> &'static str {
    match status {
        OwnedFolderStatus::Pending => "pending",
        OwnedFolderStatus::Indexing => "indexing",
        OwnedFolderStatus::Ready => "ready",
        OwnedFolderStatus::Offline => "offline",
        OwnedFolderStatus::Error => "error",
    }
}

fn run_sync_path_command(
    arguments: &[OsString],
    sync_path: impl FnOnce(&Path) -> Result<(), String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(SYNC_PATH_HELP, stdout, stderr);
    }
    // No --json: a sync reports nothing but success, so there is no
    // document for the app to parse. The exit status is the contract.
    let (paths, _) = match parse_path_arguments(arguments, false) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(stderr, "{message}\n{SYNC_PATH_USAGE}");
            return 2;
        }
    };
    if paths.is_empty() {
        let _ = writeln!(stderr, "Sync-path needs a path.\n{SYNC_PATH_USAGE}");
        return 2;
    }
    if paths.len() > 1 {
        let _ = writeln!(stderr, "Sync-path takes one path.\n{SYNC_PATH_USAGE}");
        return 2;
    }

    match sync_path(&paths[0]) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "ts sync-path: {error}");
            1
        }
    }
}

fn run_sync_rename_command(
    arguments: &[OsString],
    sync_rename: impl FnOnce(&Path, &Path) -> Result<(), String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(SYNC_RENAME_HELP, stdout, stderr);
    }
    let (paths, _) = match parse_path_arguments(arguments, false) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(stderr, "{message}\n{SYNC_RENAME_USAGE}");
            return 2;
        }
    };
    if paths.len() != 2 {
        let _ = writeln!(
            stderr,
            "Sync-rename needs the old and the new path.\n{SYNC_RENAME_USAGE}"
        );
        return 2;
    }

    match sync_rename(&paths[0], &paths[1]) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "ts sync-rename: {error}");
            1
        }
    }
}

fn run_thumbnail_command(
    arguments: &[OsString],
    make_thumbnail: impl FnOnce(&Path) -> Result<PathBuf, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(THUMBNAIL_HELP, stdout, stderr);
    }
    let (paths, json) = match parse_path_arguments(arguments, true) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(stderr, "{message}\n{THUMBNAIL_USAGE}");
            return 2;
        }
    };
    if paths.is_empty() {
        let _ = writeln!(stderr, "Thumbnail needs a path.\n{THUMBNAIL_USAGE}");
        return 2;
    }
    if paths.len() > 1 {
        let _ = writeln!(stderr, "Thumbnail takes one path.\n{THUMBNAIL_USAGE}");
        return 2;
    }

    let thumbnail = match make_thumbnail(&paths[0]) {
        Ok(thumbnail) => thumbnail,
        Err(error) => {
            let _ = writeln!(stderr, "ts thumbnail: {error}");
            return 1;
        }
    };
    if json {
        // Lossy on purpose, like every path this CLI serializes: real
        // paths can hold non-UTF-8 bytes on Unix, and those must not
        // break encoding of this document.
        // "thumbnail" is singular and "previews" plural on purpose: one
        // path here, a frame list there.
        let document = serde_json::json!({
            "schema": JSON_SCHEMA_VERSION,
            "thumbnail": thumbnail.to_string_lossy(),
        });
        return print_json(&document, "ts thumbnail", stdout, stderr);
    }
    match writeln!(stdout, "{}", thumbnail.display()).and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "ts thumbnail: could not write: {error}");
            1
        }
    }
}

fn run_preview_command(
    arguments: &[OsString],
    make_preview: impl FnOnce(&Path) -> Result<Vec<PathBuf>, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(PREVIEW_HELP, stdout, stderr);
    }
    let (paths, json) = match parse_path_arguments(arguments, true) {
        Ok(parsed) => parsed,
        Err(message) => {
            let _ = writeln!(stderr, "{message}\n{PREVIEW_USAGE}");
            return 2;
        }
    };
    if paths.is_empty() {
        let _ = writeln!(stderr, "Preview needs a path.\n{PREVIEW_USAGE}");
        return 2;
    }
    if paths.len() > 1 {
        let _ = writeln!(stderr, "Preview takes one path.\n{PREVIEW_USAGE}");
        return 2;
    }

    let frames = match make_preview(&paths[0]) {
        Ok(frames) => frames,
        Err(error) => {
            let _ = writeln!(stderr, "ts preview: {error}");
            return 1;
        }
    };
    if json {
        // Lossy paths, as in thumbnail: non-UTF-8 bytes must not break
        // encoding of this document.
        // "previews" is plural and "thumbnail" singular on purpose: a
        // frame list here, one path there.
        let frames: Vec<String> = frames
            .iter()
            .map(|frame| frame.to_string_lossy().into_owned())
            .collect();
        let document = serde_json::json!({
            "schema": JSON_SCHEMA_VERSION,
            "previews": frames,
        });
        return print_json(&document, "ts preview", stdout, stderr);
    }
    for frame in frames {
        if let Err(error) = writeln!(stdout, "{}", frame.display()) {
            let _ = writeln!(stderr, "ts preview: could not write a frame: {error}");
            return 1;
        }
    }
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "ts preview: could not write: {error}");
        return 1;
    }
    0
}

fn run_status_command(
    arguments: &[OsString],
    mut read_status: impl FnMut() -> Result<DaemonStatus, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(STATUS_HELP, stdout, stderr);
    }
    let Some((json, watch)) = parse_status_arguments(arguments) else {
        let _ = writeln!(stderr, "{STATUS_USAGE}");
        return 2;
    };
    if watch {
        // Screen clears only reach a terminal: piped watch output stays a
        // plain stream of reports, with no escape sequences in it.
        let clear_screen = !json && std::io::stdout().is_terminal();
        return run_status_watch(
            read_status,
            json,
            clear_screen,
            || std::thread::sleep(Duration::from_secs(1)),
            stdout,
            stderr,
        );
    }

    let status = match read_status() {
        Ok(status) => status,
        Err(error) => {
            let _ = writeln!(stderr, "ts status: {error}");
            return 1;
        }
    };
    print_status(&status, json, false, stdout, stderr)
}

/// Nothing, `--json`, `--watch`, or both: the whole argument language of
/// `status`. `None` is a usage error for the caller to report.
fn parse_status_arguments(arguments: &[OsString]) -> Option<(bool, bool)> {
    let mut json = false;
    let mut watch = false;
    for argument in arguments {
        if argument == OsStr::new("--json") {
            json = true;
        } else if argument == OsStr::new("--watch") {
            watch = true;
        } else {
            return None;
        }
    }
    Some((json, watch))
}

/// Prints one status snapshot. Exit 3 when the daemon is not running, so
/// scripts can branch on it; the report itself still prints. A watch
/// stream prints compact JSON, one document per line, so each snapshot
/// parses independently; a one-shot `--json` prints the pretty document.
fn print_status(
    status: &DaemonStatus,
    json: bool,
    compact_json: bool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let stopped = if status.running { 0 } else { 3 };
    if json {
        // The struct already serializes to this document's shape; only the
        // schema version is the CLI's own. Paths inside serialize lossy, so
        // non-UTF-8 names cannot break encoding.
        let mut document = match serde_json::to_value(status) {
            Ok(document) => document,
            Err(error) => {
                let _ = writeln!(stderr, "ts status: could not encode results: {error}");
                return 1;
            }
        };
        document["schema"] = serde_json::json!(JSON_SCHEMA_VERSION);
        let code = if compact_json {
            print_json_compact(&document, "ts status", stdout, stderr)
        } else {
            print_json(&document, "ts status", stdout, stderr)
        };
        return match code {
            0 => stopped,
            code => code,
        };
    }
    let report = daemon_state::render(status);
    match write!(stdout, "{report}").and_then(|()| stdout.flush()) {
        Ok(()) => stopped,
        Err(error) => {
            let _ = writeln!(stderr, "ts status: could not write: {error}");
            1
        }
    }
}

/// How many times `--watch` retries a failed status read before giving
/// up. Each retry waits out the normal one-second cadence, so a transient
/// failure (a locked database mid-scan) rides out about five seconds of
/// trouble; anything longer is reported and exits 1.
const WATCH_READ_RETRIES: u32 = 5;

/// Whether `--watch` is done: the daemon reports idle (or waiting on
/// offline storage alone) with nothing actionable queued. Offline folders
/// never hold the wait; an unknown activity keeps watching, since only a
/// known-idle daemon proves the queue drained.
fn watch_is_done(status: &DaemonStatus) -> bool {
    status.running
        && status.queue.is_empty()
        && matches!(
            status.activity,
            Some(DaemonActivity::Idle | DaemonActivity::OfflineWaiting)
        )
}

/// Re-reads the daemon status until the queue drains or the daemon stops:
/// `--watch`. Human output clears the screen between renders when it
/// reaches a terminal; piped output and `--json` stay plain streams, JSON
/// as one compact document per line.
fn run_status_watch(
    mut read_status: impl FnMut() -> Result<DaemonStatus, String>,
    json: bool,
    clear_screen: bool,
    mut wait: impl FnMut(),
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let mut first = true;
    let mut failures = 0;
    loop {
        let status = match read_status() {
            Ok(status) => {
                failures = 0;
                status
            }
            Err(error) => {
                // A failed read retries silently: only a failure that
                // outlasts the retry bound is worth reporting.
                failures += 1;
                if failures > WATCH_READ_RETRIES {
                    let _ = writeln!(stderr, "ts status: {error}");
                    return 1;
                }
                wait();
                continue;
            }
        };
        if clear_screen && !first {
            // Re-render in place; piped output never sees these escapes.
            if write!(stdout, "\x1b[2J\x1b[H").is_err() {
                let _ = writeln!(stderr, "ts status: could not write");
                return 1;
            }
        }
        first = false;
        // Compact exactly when streaming JSON: human watch output is
        // unaffected.
        let code = print_status(&status, json, json, stdout, stderr);
        if code != 0 {
            // A write or encode error (1), or a stopped daemon (3).
            return code;
        }
        if watch_is_done(&status) {
            return 0;
        }
        wait();
    }
}

fn run_daemon_command(
    arguments: &[OsString],
    run_daemon: impl FnOnce() -> Result<(), String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(DAEMON_HELP, stdout, stderr);
    }
    let Some((subcommand, rest)) = arguments.split_first() else {
        let _ = writeln!(stderr, "{DAEMON_HELP}");
        return 2;
    };
    if subcommand == OsStr::new("run") {
        return run_daemon_run_command(rest, run_daemon, stdout, stderr);
    }
    let _ = writeln!(stderr, "{DAEMON_HELP}");
    2
}

fn run_daemon_run_command(
    arguments: &[OsString],
    run_daemon: impl FnOnce() -> Result<(), String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(DAEMON_RUN_HELP, stdout, stderr);
    }
    if !arguments.is_empty() {
        let _ = writeln!(stderr, "{DAEMON_RUN_USAGE}");
        return 2;
    }
    match run_daemon() {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "ts daemon run: {error}");
            1
        }
    }
}

/// Paths plus an optional `--json`: the argument language of the small
/// path-taking commands. `--` ends option parsing so a path starting with
/// a dash stays addressable, exactly as in search. The caller checks the
/// arity it needs; an unknown option is an error to report with usage.
/// An empty path is rejected outright: it almost always means an unset
/// variable expanded to nothing, and silently resolving it against the
/// working directory would retarget the command at the wrong folder.
fn parse_path_arguments(
    arguments: &[OsString],
    json_allowed: bool,
) -> Result<(Vec<PathBuf>, bool), String> {
    let mut paths = Vec::new();
    let mut json = false;
    let mut only_paths = false;
    for argument in arguments {
        if argument.is_empty() {
            return Err("A path must not be empty.".to_owned());
        }
        if only_paths {
            paths.push(PathBuf::from(argument));
            continue;
        }
        if argument == OsStr::new("--") {
            only_paths = true;
            continue;
        }
        if argument == OsStr::new("--json") {
            if !json_allowed {
                return Err("Unknown option '--json'.".to_owned());
            }
            json = true;
            continue;
        }
        if let Some(text) = argument.to_str() {
            if text.starts_with('-') {
                return Err(format!("Unknown option '{text}'."));
            }
        }
        // Anything else is a path, including names no UTF-8 check could
        // read: real paths on Unix can hold non-UTF-8 bytes.
        paths.push(PathBuf::from(argument));
    }
    Ok((paths, json))
}

/// Nothing or `--json`: the whole argument language of the small read-only
/// commands. `None` is a usage error for the caller to report.
fn parse_json_flag(arguments: &[OsString]) -> Option<bool> {
    if arguments.is_empty() {
        return Some(false);
    }
    if arguments.len() == 1 && arguments[0] == OsStr::new("--json") {
        return Some(true);
    }
    None
}

fn run_stats_command(
    arguments: &[OsString],
    read_index_stats: impl FnOnce() -> Result<IndexStats, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(STATS_HELP, stdout, stderr);
    }
    let Some(json) = parse_json_flag(arguments) else {
        let _ = writeln!(stderr, "{STATS_USAGE}");
        return 2;
    };

    let stats = match read_index_stats() {
        Ok(stats) => stats,
        Err(error) => {
            let _ = writeln!(stderr, "ts stats: {error}");
            return 1;
        }
    };
    if json {
        // The struct already serializes to this document's shape; only the
        // schema version is the CLI's own. Paths inside serialize lossy, so
        // non-UTF-8 folder names cannot break encoding.
        let mut document = match serde_json::to_value(&stats) {
            Ok(document) => document,
            Err(error) => {
                let _ = writeln!(stderr, "ts stats: could not encode results: {error}");
                return 1;
            }
        };
        document["schema"] = serde_json::json!(JSON_SCHEMA_VERSION);
        return print_json(&document, "ts stats", stdout, stderr);
    }
    let report = index_stats::render(&stats);
    match write!(stdout, "{report}").and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "ts stats: could not write: {error}");
            1
        }
    }
}

struct SearchOptions {
    query: String,
    limit: Option<usize>,
    page: Option<usize>,
    media_type: MediaType,
    count: bool,
    json: bool,
    verbose: bool,
}

/// A search hit with the full path the engine resolved it to. The rest is
/// the engine's own result shape, so the JSON contract stays one shape.
#[derive(Serialize)]
struct SearchHit {
    // Lossy on purpose: real paths can hold non-UTF-8 bytes on Unix, and
    // those must not break serialization of this document.
    #[serde(serialize_with = "toka_search::lossy_path::serialize")]
    path: PathBuf,
    #[serde(flatten)]
    hit: VideoResult,
}

struct SearchSummary {
    total_results: usize,
    total_pages: usize,
    index_revision: u64,
}

fn run_search_command(
    arguments: &[OsString],
    create_engine: impl FnOnce() -> Result<SearchEngine, String>,
    create_verbose_engine: impl FnOnce() -> Result<SearchEngine, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(&search_help(), stdout, stderr);
    }

    let options = match parse_search_arguments(arguments) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(stderr, "{message}\n{SEARCH_USAGE}");
            return 2;
        }
    };

    let engine = match if options.verbose {
        create_verbose_engine()
    } else {
        create_engine()
    } {
        Ok(engine) => engine,
        Err(error) => {
            let _ = writeln!(stderr, "ts search: {error}");
            return 1;
        }
    };
    if options.count {
        return run_search_count(&options, &engine, stdout, stderr);
    }
    run_search(&options, &engine, stdout, stderr)
}

/// Streams the trace the last search recorded to stderr, so `--verbose`
/// shows it while stdout keeps clean results.
fn write_verbose_trace(engine: &SearchEngine, stderr: &mut impl Write) -> std::io::Result<()> {
    let trace = engine.take_verbose_trace();
    write!(stderr, "{trace}").and_then(|()| stderr.flush())
}

fn run_search_count(
    options: &SearchOptions,
    engine: &SearchEngine,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let request = SearchRequest {
        query: options.query.clone(),
        page: 1,
        page_size: PAGE_SIZE,
        search_id: None,
        fields: SearchFields::default(),
        media_type: options.media_type,
    };
    match engine.match_count(request) {
        Ok(count) => {
            if options.verbose {
                if let Err(error) = write_verbose_trace(engine, stderr) {
                    let _ = writeln!(
                        stderr,
                        "ts search: could not write the search trace: {error}"
                    );
                    return 1;
                }
            }
            if options.json {
                let document = serde_json::json!({
                    "schema": JSON_SCHEMA_VERSION,
                    "query": options.query,
                    "totalResults": count,
                });
                return print_json(&document, "ts search", stdout, stderr);
            }
            if let Err(error) = writeln!(stdout, "{count}").and_then(|()| stdout.flush()) {
                let _ = writeln!(stderr, "ts search: could not write the count: {error}");
                return 1;
            }
            0
        }
        Err(error) => {
            if options.verbose {
                let _ = write_verbose_trace(engine, stderr);
            }
            let _ = writeln!(stderr, "ts search: {error}");
            1
        }
    }
}

fn print_json(
    document: &serde_json::Value,
    command: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    match serde_json::to_string_pretty(document) {
        Ok(text) => write_json_line(&text, command, stdout, stderr),
        Err(error) => {
            let _ = writeln!(stderr, "{command}: could not encode results: {error}");
            1
        }
    }
}

/// One compact document per line, for `--watch` streams: line-oriented
/// consumers parse each snapshot independently.
fn print_json_compact(
    document: &serde_json::Value,
    command: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    match serde_json::to_string(document) {
        Ok(text) => write_json_line(&text, command, stdout, stderr),
        Err(error) => {
            let _ = writeln!(stderr, "{command}: could not encode results: {error}");
            1
        }
    }
}

fn write_json_line(
    text: &str,
    command: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if let Err(error) = writeln!(stdout, "{text}").and_then(|()| stdout.flush()) {
        let _ = writeln!(stderr, "{command}: could not write results: {error}");
        return 1;
    }
    0
}

fn run_search(
    options: &SearchOptions,
    engine: &SearchEngine,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    // One call fetches every match; the CLI sorts the complete set by
    // path and only then slices the requested page, which keeps every
    // page stable across invocations. Pages past the last slice to
    // nothing.
    let found = match engine.search_all(
        options.query.clone(),
        SearchFields::default(),
        options.media_type,
    ) {
        Ok(found) => found,
        Err(error) => {
            if options.verbose {
                let _ = write_verbose_trace(engine, stderr);
            }
            let _ = writeln!(stderr, "ts search: {error}");
            return 1;
        }
    };
    if options.verbose {
        if let Err(error) = write_verbose_trace(engine, stderr) {
            let _ = writeln!(
                stderr,
                "ts search: could not write the search trace: {error}"
            );
            return 1;
        }
    }
    let summary = SearchSummary {
        total_results: found.total_results,
        total_pages: found.total_pages,
        index_revision: found.index_revision,
    };

    let mut hits = Vec::new();
    for result in found.results {
        if let Ok(path) = engine.video_path(&result.id) {
            hits.push(SearchHit { path, hit: result });
        }
    }

    hits.sort_by(|left, right| left.path.cmp(&right.path));
    hits.dedup_by(|left, right| left.path == right.path);
    if let Some(wanted) = options.page {
        let start = (wanted - 1).saturating_mul(PAGE_SIZE);
        hits = hits.into_iter().skip(start).take(PAGE_SIZE).collect();
    }
    if let Some(limit) = options.limit {
        hits.truncate(limit);
    }
    if options.json {
        // The totals describe the whole search; the page fields describe
        // this document, which holds one requested page or every hit, and
        // pageSize counts the hits this document actually holds.
        let (page, total_pages) = match options.page {
            Some(wanted) => (wanted, summary.total_pages),
            None => (1, 1),
        };
        let page_size = hits.len();
        // Encode the hits fallibly (see folders list): a serialization
        // failure exits 1 instead of panicking inside `json!`.
        let hits = match serde_json::to_value(&hits) {
            Ok(hits) => hits,
            Err(error) => {
                let _ = writeln!(stderr, "ts search: could not encode results: {error}");
                return 1;
            }
        };
        let document = serde_json::json!({
            "schema": JSON_SCHEMA_VERSION,
            "query": options.query,
            "page": page,
            "pageSize": page_size,
            "totalResults": summary.total_results,
            "totalPages": total_pages,
            "indexRevision": summary.index_revision,
            "results": hits,
        });
        return print_json(&document, "ts search", stdout, stderr);
    }
    for hit in hits {
        if let Err(error) = writeln!(stdout, "{}", hit.path.display()) {
            let _ = writeln!(stderr, "ts search: could not write a result: {error}");
            return 1;
        }
    }
    if let Err(error) = stdout.flush() {
        let _ = writeln!(stderr, "ts search: could not write results: {error}");
        return 1;
    }
    0
}

fn parse_search_arguments(arguments: &[OsString]) -> Result<SearchOptions, String> {
    let mut options = SearchOptions {
        query: String::new(),
        limit: None,
        page: None,
        media_type: MediaType::Both,
        count: false,
        json: false,
        verbose: false,
    };
    let mut words: Vec<&str> = Vec::new();
    let mut only_words = false;
    let mut index = 0;
    while index < arguments.len() {
        let Some(text) = arguments[index].to_str() else {
            return Err("Search terms must be valid text.".to_owned());
        };
        if only_words {
            words.push(text);
        } else if text == "--" {
            only_words = true;
        } else if text == "--limit" {
            options.limit = Some(take_search_number("--limit", arguments, &mut index)?);
        } else if text == "--page" {
            options.page = Some(take_search_number("--page", arguments, &mut index)?);
        } else if text == "--count" {
            options.count = true;
        } else if text == "--json" {
            options.json = true;
        } else if text == "--verbose" {
            options.verbose = true;
        } else if text == "--type" {
            index += 1;
            let value = arguments
                .get(index)
                .and_then(|value| value.to_str())
                .ok_or_else(|| "--type needs one of: video, image, all.".to_owned())?;
            options.media_type = match value {
                "video" => MediaType::Videos,
                "image" => MediaType::Images,
                "all" => MediaType::Both,
                _ => {
                    return Err(format!(
                        "Unknown type '{value}'. Use one of: video, image, all."
                    ));
                }
            };
        } else if text.starts_with('-') {
            return Err(format!("Unknown option '{text}'."));
        } else {
            words.push(text);
        }
        index += 1;
    }
    let query = words.join(" ");
    if query.trim().is_empty() {
        return Err("Search needs a query.".to_owned());
    }
    if options.count && (options.page.is_some() || options.limit.is_some()) {
        return Err("--count cannot be combined with --page or --limit.".to_owned());
    }
    options.query = query;
    Ok(options)
}

fn take_search_number(
    flag: &str,
    arguments: &[OsString],
    index: &mut usize,
) -> Result<usize, String> {
    *index += 1;
    let value = arguments
        .get(*index)
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("{flag} needs a positive number."))?;
    value
        .parse::<usize>()
        .ok()
        .filter(|number| *number >= 1)
        .ok_or_else(|| format!("{flag} needs a positive number, got '{value}'."))
}

fn print_text(text: &str, stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    if let Err(error) = writeln!(stdout, "{text}").and_then(|()| stdout.flush()) {
        let _ = writeln!(stderr, "ts: could not write output: {error}");
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::run_command;
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };
    use tempfile::tempdir;
    use toka_search::{
        daemon_state::{DaemonActivity, DaemonQueue, DaemonStatus},
        index_stats::{FolderState, FolderStats, IndexStats},
        owned_index::{OwnedFolderStatus, OwnedIndexFolder, OwnedIndexState, ScannedDirectory},
        search::{SearchEngine, SearchError, SearchProvider},
        search_log::SearchLogContext,
    };

    fn no_engine() -> Result<SearchEngine, String> {
        panic!("this command must not create the search engine")
    }

    fn no_verbose_engine() -> Result<SearchEngine, String> {
        panic!("this command must not create the verbose search engine")
    }

    fn no_stats() -> Result<IndexStats, String> {
        panic!("this command must not read index stats")
    }

    fn no_folders() -> Result<Vec<OwnedIndexFolder>, String> {
        panic!("this command must not read folders")
    }

    fn no_add_folder(_path: &Path) -> Result<(OwnedIndexState, PathBuf), String> {
        panic!("this command must not add folders")
    }

    fn no_remove_folder(_id: &str) -> Result<OwnedIndexState, String> {
        panic!("this command must not remove folders")
    }

    fn no_sync_path(_path: &Path) -> Result<(), String> {
        panic!("this command must not sync paths")
    }

    fn no_sync_rename(_from: &Path, _to: &Path) -> Result<(), String> {
        panic!("this command must not sync renames")
    }

    fn no_thumbnail(_path: &Path) -> Result<PathBuf, String> {
        panic!("this command must not make thumbnails")
    }

    fn no_preview(_path: &Path) -> Result<Vec<PathBuf>, String> {
        panic!("this command must not make previews")
    }

    fn no_status() -> Result<DaemonStatus, String> {
        panic!("this command must not read the daemon status")
    }

    fn no_daemon_run() -> Result<(), String> {
        panic!("this command must not run the daemon")
    }

    #[test]
    fn version_prefers_the_build_version_over_the_manifest_version() {
        assert_eq!(
            super::pick_version(Some("0.1.1+482.a1b2c3d.feat")),
            "0.1.1+482.a1b2c3d.feat"
        );
        assert_eq!(super::pick_version(None), env!("CARGO_PKG_VERSION"));
        assert_eq!(super::pick_version(Some("")), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn version_text_includes_the_git_sha_or_development() {
        assert_eq!(
            super::version_text("0.1.1+5.abc", Some("abc123")),
            "ts 0.1.1+5.abc (Git SHA abc123)"
        );
        assert_eq!(
            super::version_text("0.1.1", None),
            "ts 0.1.1 (Git SHA development)"
        );
        assert_eq!(
            super::version_text("0.1.1", Some("")),
            super::version_text("0.1.1", None)
        );
    }

    #[test]
    fn help_flags_print_without_building_anything() {
        for flag in ["-h", "--help"] {
            let args = ["ts", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            let output = String::from_utf8(stdout).unwrap();
            assert!(output.contains("Usage: ts <command>"), "{output}");
            for command in [
                "search",
                "stats",
                "status",
                "folders",
                "sync-path",
                "sync-rename",
                "thumbnail",
                "preview",
                "--version",
            ] {
                assert!(output.contains(command), "{output}");
            }
            // The service manager's entry point stays hidden: no
            // command row of its own (the status row still names it).
            assert!(!output.contains("\n  daemon"), "{output}");
            assert!(stderr.is_empty());
        }
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

    struct ErrorProvider;

    impl SearchProvider for ErrorProvider {
        fn candidates(
            &self,
            _query: &str,
            _whole_path: bool,
            _context: &SearchLogContext,
        ) -> Result<Vec<PathBuf>, SearchError> {
            Err(SearchError::Provider("backend unavailable".into()))
        }
    }

    fn verbose_engine(provider: impl SearchProvider + 'static) -> SearchEngine {
        SearchEngine::new_with_logger(
            Arc::new(provider),
            Arc::new(toka_search::search_log::SearchLogger::with_verbose_capture()),
        )
    }

    #[test]
    fn search_command_prints_full_matching_paths() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("family-video.mp4");
        fs::write(&video, b"video").unwrap();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(vec![video.clone()])));
        let args = ["ts", "search", "family video"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
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
    fn search_without_a_query_prints_usage() {
        let args = ["ts", "search"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("Search needs a query.\n{}\n", super::SEARCH_USAGE)
        );
    }

    #[test]
    fn search_help_does_not_create_the_search_engine() {
        for flag in ["--help", "-h"] {
            let args = ["ts", "search", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                || panic!("help must not create the search engine"),
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            assert!(String::from_utf8(stdout)
                .unwrap()
                .contains(super::SEARCH_USAGE));
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn search_engine_setup_error_returns_a_failure_status() {
        let args = ["ts", "search", "family"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Err("search index could not be opened".into()),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts search: search index could not be opened\n"
        );
    }

    #[test]
    fn search_type_filters_to_videos_or_images() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("family-video.mp4");
        let image = directory.path().join("family-image.jpg");
        fs::write(&video, b"video").unwrap();
        fs::write(&image, b"image").unwrap();
        for (flag, expected) in [
            ("video", vec![video.clone()]),
            ("image", vec![image.clone()]),
            ("all", vec![image.clone(), video.clone()]),
        ] {
            let engine = SearchEngine::new(Arc::new(FixtureProvider(vec![
                video.clone(),
                image.clone(),
            ])));
            let args = ["ts", "search", "family", "--type", flag]
                .map(OsString::from)
                .to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                || Ok(engine),
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            let expected = expected
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            assert_eq!(String::from_utf8(stdout).unwrap(), expected, "{flag}");
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn search_limit_caps_the_printed_results() {
        let directory = tempdir().unwrap();
        let mut videos = (1..=3)
            .map(|number| {
                let path = directory.path().join(format!("family-0{number}.mp4"));
                fs::write(&path, b"video").unwrap();
                path
            })
            .collect::<Vec<_>>();
        videos.sort();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos.clone())));
        let args = ["ts", "search", "family", "--limit", "2"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let expected = videos
            .iter()
            .take(2)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert_eq!(String::from_utf8(stdout).unwrap(), expected);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_page_prints_only_that_page() {
        let directory = tempdir().unwrap();
        let videos = (0..25)
            .map(|number| {
                let path = directory
                    .path()
                    .join(format!("family-trip-{number:02}.mp4"));
                fs::write(&path, b"video").unwrap();
                path
            })
            .collect::<Vec<_>>();
        for (page, expected_lines) in [("1", 24), ("2", 1)] {
            let engine = SearchEngine::new(Arc::new(FixtureProvider(videos.clone())));
            let args = ["ts", "search", "family", "--page", page]
                .map(OsString::from)
                .to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                || Ok(engine),
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "page {page}");
            let output = String::from_utf8(stdout).unwrap();
            let lines = output.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), expected_lines, "page {page}");
            for line in lines {
                assert!(
                    videos
                        .iter()
                        .any(|video| video.display().to_string() == line),
                    "unexpected path: {line}"
                );
            }
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn search_pages_come_from_the_globally_sorted_set() {
        let directory = tempdir().unwrap();
        let mut videos = (0..25)
            .map(|number| {
                let path = directory
                    .path()
                    .join(format!("family-trip-{number:02}.mp4"));
                fs::write(&path, b"video").unwrap();
                path
            })
            .collect::<Vec<_>>();
        videos.sort();
        // Candidates arrive least-sorted first, so the pages can only be
        // right if the CLI sorts the complete set before slicing.
        let candidates = videos.iter().rev().cloned().collect::<Vec<_>>();
        let mut pages = Vec::new();
        for page in ["1", "2"] {
            // A fresh engine per page, as separate invocations would have.
            let engine = SearchEngine::new(Arc::new(FixtureProvider(candidates.clone())));
            let args = ["ts", "search", "family", "--page", page]
                .map(OsString::from)
                .to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                || Ok(engine),
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "page {page}");
            assert!(stderr.is_empty());
            pages.push(String::from_utf8(stdout).unwrap());
        }
        let paths = videos
            .iter()
            .map(|video| video.display().to_string())
            .collect::<Vec<_>>();
        assert_eq!(pages[0], paths[..24].join("\n") + "\n", "page 1");
        assert_eq!(pages[1], format!("{}\n", paths[24]), "page 2");
    }

    #[test]
    fn search_page_past_the_last_page_prints_nothing() {
        let directory = tempdir().unwrap();
        let videos = (0..2)
            .map(|number| {
                let path = directory
                    .path()
                    .join(format!("family-trip-{number:02}.mp4"));
                fs::write(&path, b"video").unwrap();
                path
            })
            .collect::<Vec<_>>();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos.clone())));
        let args = ["ts", "search", "family", "--page", "5"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());

        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos)));
        let args = ["ts", "search", "family", "--page", "5", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let document: serde_json::Value =
            serde_json::from_str(&String::from_utf8(stdout).unwrap()).unwrap();
        assert_eq!(document["page"], 5);
        assert_eq!(document["pageSize"], 0);
        assert_eq!(document["totalResults"], 2);
        assert_eq!(document["results"].as_array().unwrap().len(), 0);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_limit_applies_after_page_selection() {
        let directory = tempdir().unwrap();
        let mut videos = (0..25)
            .map(|number| {
                let path = directory
                    .path()
                    .join(format!("family-trip-{number:02}.mp4"));
                fs::write(&path, b"video").unwrap();
                path
            })
            .collect::<Vec<_>>();
        videos.sort();
        let paths = videos
            .iter()
            .map(|video| video.display().to_string())
            .collect::<Vec<_>>();
        for (page, limit, expected) in [
            ("1", "2", paths[..2].join("\n") + "\n"),
            ("2", "5", format!("{}\n", paths[24])),
        ] {
            let engine = SearchEngine::new(Arc::new(FixtureProvider(videos.clone())));
            let args = ["ts", "search", "family", "--page", page, "--limit", limit]
                .map(OsString::from)
                .to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                || Ok(engine),
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "page {page} limit {limit}");
            assert_eq!(
                String::from_utf8(stdout).unwrap(),
                expected,
                "page {page} limit {limit}"
            );
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn search_count_prints_only_the_match_count() {
        let directory = tempdir().unwrap();
        let videos = ["family-one.mp4", "family-two.mp4", "unrelated.mp4"]
            .map(|name| {
                let path = directory.path().join(name);
                fs::write(&path, b"video").unwrap();
                path
            })
            .to_vec();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos)));
        let args = ["ts", "search", "family", "--count"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(String::from_utf8(stdout).unwrap(), "2\n");
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_count_with_page_or_limit_is_a_usage_error() {
        for arguments in [
            ["ts", "search", "family", "--count", "--page", "1"],
            ["ts", "search", "family", "--count", "--limit", "1"],
        ] {
            let args = arguments.map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            let error = String::from_utf8(stderr).unwrap();
            assert!(error.contains("--count cannot be combined"), "{error}");
            assert!(error.contains(super::SEARCH_USAGE), "{error}");
        }
    }

    #[test]
    fn search_json_prints_a_versioned_document() {
        let directory = tempdir().unwrap();
        let mut videos = ["family-one.mp4", "family-two.mp4"]
            .map(|name| {
                let path = directory.path().join(name);
                fs::write(&path, b"video").unwrap();
                path
            })
            .to_vec();
        videos.sort();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos.clone())));
        let args = ["ts", "search", "family", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["query"], "family");
        assert_eq!(document["page"], 1);
        assert_eq!(document["pageSize"], 2);
        assert_eq!(document["totalResults"], 2);
        assert_eq!(document["totalPages"], 1);
        assert!(document["indexRevision"].is_number());
        let results = document["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        for (result, video) in results.iter().zip(videos.iter()) {
            assert_eq!(result["path"], video.display().to_string());
            assert!(!result["id"].as_str().unwrap().is_empty());
            assert_eq!(result["extension"], "mp4");
            assert!(result["tags"].is_array());
            assert!(result["folderTags"].is_array());
        }
        assert_eq!(results[0]["fileName"], "family-one.mp4");
        assert_eq!(results[1]["fileName"], "family-two.mp4");
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_json_with_page_reports_that_page() {
        let directory = tempdir().unwrap();
        let videos = (0..25)
            .map(|number| {
                let path = directory
                    .path()
                    .join(format!("family-trip-{number:02}.mp4"));
                fs::write(&path, b"video").unwrap();
                path
            })
            .collect::<Vec<_>>();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos.clone())));
        let args = ["ts", "search", "family", "--page", "2", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["page"], 2);
        // pageSize counts this document's hits, not the engine's page.
        assert_eq!(document["pageSize"], 1);
        assert_eq!(document["totalResults"], 25);
        assert_eq!(document["totalPages"], 2);
        let results = document["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        let mut videos = videos.clone();
        videos.sort();
        assert_eq!(
            results[0]["path"],
            videos[24].display().to_string(),
            "page 2 holds the last path of the sorted set"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_json_page_size_counts_the_hits_in_the_document() {
        let directory = tempdir().unwrap();
        let videos = (0..3)
            .map(|number| {
                let path = directory
                    .path()
                    .join(format!("family-trip-{number:02}.mp4"));
                fs::write(&path, b"video").unwrap();
                path
            })
            .collect::<Vec<_>>();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos)));
        let args = ["ts", "search", "family", "--json", "--limit", "2"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let document: serde_json::Value =
            serde_json::from_str(&String::from_utf8(stdout).unwrap()).unwrap();
        assert_eq!(document["page"], 1);
        assert_eq!(document["pageSize"], 2);
        assert_eq!(document["totalResults"], 3);
        assert_eq!(document["results"].as_array().unwrap().len(), 2);
        assert!(stderr.is_empty());
    }

    // Linux-only: only filesystems like ext4/tmpfs accept non-UTF-8 names;
    // macOS (APFS) rejects them at creation, so this scenario cannot occur
    // there. The lossy conversion itself is platform-neutral.
    #[cfg(target_os = "linux")]
    #[test]
    fn search_json_with_non_utf8_names_prints_lossy_paths() {
        use std::os::unix::ffi::OsStringExt;
        let directory = tempdir().unwrap();
        let name = OsString::from_vec(b"family-\xff-trip.mp4".to_vec());
        let video = directory.path().join(&name);
        fs::write(&video, b"video").unwrap();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(vec![video.clone()])));
        let args = ["ts", "search", "family", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let document: serde_json::Value =
            serde_json::from_str(&String::from_utf8(stdout).unwrap()).unwrap();
        assert_eq!(document["totalResults"], 1);
        let results = document["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["path"], video.to_string_lossy().into_owned());
        assert_eq!(
            results[0]["fileName"],
            video.file_name().unwrap().to_string_lossy().into_owned()
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_json_with_no_matches_is_an_empty_document() {
        let engine = SearchEngine::new(Arc::new(FixtureProvider(Vec::new())));
        let args = ["ts", "search", "missing", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["totalResults"], 0);
        assert_eq!(document["results"].as_array().unwrap().len(), 0);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_json_count_prints_total_results() {
        let directory = tempdir().unwrap();
        let videos = ["family-one.mp4", "family-two.mp4", "unrelated.mp4"]
            .map(|name| {
                let path = directory.path().join(name);
                fs::write(&path, b"video").unwrap();
                path
            })
            .to_vec();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(videos)));
        let args = ["ts", "search", "family", "--count", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["query"], "family");
        assert_eq!(document["totalResults"], 2);
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_verbose_streams_the_trace_to_stderr() {
        let directory = tempdir().unwrap();
        let matched = directory.path().join("summer-trip.mp4");
        fs::write(&matched, b"video").unwrap();
        let skipped = directory.path().join("winter-trip.mp4");
        let engine = verbose_engine(FixtureProvider(vec![matched.clone(), skipped.clone()]));
        let args = ["ts", "search", "summer", "--verbose"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || panic!("verbose search must not create the plain engine"),
            || Ok(engine),
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!("{}\n", matched.display())
        );
        let trace = String::from_utf8(stderr).unwrap();
        assert!(
            trace.contains(&format!(
                "FILTERED OUT: {} (query did not match)",
                skipped.display()
            )),
            "{trace}"
        );
        assert!(trace.contains("RESULTS RETURNED TO APP:"), "{trace}");
        assert!(trace.contains(&matched.display().to_string()), "{trace}");
    }

    #[test]
    fn search_verbose_count_streams_filtered_paths() {
        let directory = tempdir().unwrap();
        let matched = directory.path().join("summer-trip.mp4");
        fs::write(&matched, b"video").unwrap();
        let skipped = directory.path().join("winter-trip.mp4");
        let engine = verbose_engine(FixtureProvider(vec![matched, skipped.clone()]));
        let args = ["ts", "search", "summer", "--count", "--verbose"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || panic!("verbose search must not create the plain engine"),
            || Ok(engine),
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(String::from_utf8(stdout).unwrap(), "1\n");
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains(&format!("FILTERED OUT: {}", skipped.display())));
    }

    #[test]
    fn search_verbose_json_keeps_the_trace_on_stderr() {
        let directory = tempdir().unwrap();
        let matched = directory.path().join("summer-trip.mp4");
        fs::write(&matched, b"video").unwrap();
        let skipped = directory.path().join("winter-trip.mp4");
        let engine = verbose_engine(FixtureProvider(vec![matched.clone(), skipped.clone()]));
        let args = ["ts", "search", "summer", "--verbose", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || panic!("verbose search must not create the plain engine"),
            || Ok(engine),
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let document: serde_json::Value =
            serde_json::from_str(&String::from_utf8(stdout).unwrap()).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["totalResults"], 1);
        let results = document["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["path"], matched.display().to_string());
        let trace = String::from_utf8(stderr).unwrap();
        assert!(
            trace.contains(&format!("FILTERED OUT: {}", skipped.display())),
            "{trace}"
        );
        assert!(trace.contains("RESULTS RETURNED TO APP:"), "{trace}");
    }

    #[test]
    fn search_verbose_reports_errors_with_the_trace() {
        let engine = verbose_engine(ErrorProvider);
        let args = ["ts", "search", "family", "--verbose"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || panic!("verbose search must not create the plain engine"),
            || Ok(engine),
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        let error = String::from_utf8(stderr).unwrap();
        assert!(
            error.contains("SEARCH ERROR: backend unavailable"),
            "{error}"
        );
        assert!(error.contains("ts search: backend unavailable"), "{error}");
    }

    #[test]
    fn search_provider_error_returns_a_failure_status() {
        let engine = SearchEngine::new(Arc::new(ErrorProvider));
        let args = ["ts", "search", "family"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts search: backend unavailable\n"
        );
    }

    #[test]
    fn search_with_an_unknown_type_is_a_usage_error() {
        let args = ["ts", "search", "family", "--type", "audio"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        let error = String::from_utf8(stderr).unwrap();
        assert!(error.contains("Unknown type 'audio'"), "{error}");
        assert!(error.contains(super::SEARCH_USAGE), "{error}");
    }

    #[test]
    fn search_with_an_unknown_option_is_a_usage_error() {
        let args = ["ts", "search", "family", "--bogus"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        let error = String::from_utf8(stderr).unwrap();
        assert!(error.contains("Unknown option '--bogus'"), "{error}");
        assert!(error.contains(super::SEARCH_USAGE), "{error}");
    }

    #[test]
    fn search_numbers_must_be_positive() {
        for arguments in [
            vec!["ts", "search", "family", "--limit"],
            vec!["ts", "search", "family", "--limit", "zero"],
            vec!["ts", "search", "family", "--limit", "0"],
            vec!["ts", "search", "family", "--page"],
            vec!["ts", "search", "family", "--page", "1.5"],
            vec!["ts", "search", "family", "--page", "0"],
            vec!["ts", "search", "family", "--type"],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            assert!(
                String::from_utf8(stderr)
                    .unwrap()
                    .contains(super::SEARCH_USAGE),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn search_with_only_flags_still_needs_a_query() {
        let args = ["ts", "search", "--type", "video"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("Search needs a query.\n{}\n", super::SEARCH_USAGE)
        );
    }

    #[test]
    fn search_after_a_dash_separator_takes_flags_literally() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("--limit-fan.mp4");
        fs::write(&video, b"video").unwrap();
        let engine = SearchEngine::new(Arc::new(FixtureProvider(vec![video.clone()])));
        let args = ["ts", "search", "--", "--limit-fan"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            || Ok(engine),
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
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

    fn sample_stats() -> IndexStats {
        IndexStats {
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
        }
    }

    #[test]
    fn stats_prints_the_report() {
        let args = ["ts", "stats"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            || Ok(sample_stats()),
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let text = String::from_utf8(stdout).unwrap();
        assert!(
            text.contains("Videos:   4") && text.contains("/media"),
            "{text}"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn stats_json_prints_a_versioned_document() {
        let args = ["ts", "stats", "--json"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            || Ok(sample_stats()),
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["revision"], 1);
        assert_eq!(document["videos"], 4);
        assert_eq!(document["images"], 1);
        assert_eq!(document["serviceRunning"], true);
        assert_eq!(document["folders"][0]["path"], "/media");
        assert_eq!(document["folders"][0]["state"], "ready");
        // The serialized struct carries exactly the documented keys.
        assert_eq!(document.as_object().unwrap().len(), 6);
        assert!(stderr.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn stats_json_with_non_utf8_folder_prints_lossy_paths() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(b"/media/movies-\xff".to_vec());
        let mut stats = sample_stats();
        stats.folders[0].path = PathBuf::from(raw);
        let expected = stats.folders[0].path.to_string_lossy().into_owned();
        let args = ["ts", "stats", "--json"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            || Ok(stats),
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["folders"][0]["path"], expected);
        assert!(stderr.is_empty());
    }

    #[test]
    fn stats_help_does_not_read_stats() {
        for flag in ["--help", "-h"] {
            let args = ["ts", "stats", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            assert!(String::from_utf8(stdout)
                .unwrap()
                .contains(super::STATS_USAGE));
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn stats_failure_and_extra_arguments_are_errors() {
        let args = ["ts", "stats"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            || Err("no index".to_owned()),
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, 1);
        assert_eq!(String::from_utf8(stderr).unwrap(), "ts stats: no index\n");

        let args = ["ts", "stats", "extra"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("{}\n", super::STATS_USAGE)
        );
    }

    fn sample_folders() -> Vec<OwnedIndexFolder> {
        vec![
            OwnedIndexFolder {
                id: "abc".into(),
                path: "/media/movies".into(),
                status: OwnedFolderStatus::Ready,
                message: None,
                scanned_files: 0,
                current_path: None,
                recent_directories: Vec::new(),
            },
            OwnedIndexFolder {
                id: "def".into(),
                path: "/media/gone".into(),
                status: OwnedFolderStatus::Offline,
                message: Some("not mounted".into()),
                scanned_files: 0,
                current_path: None,
                recent_directories: Vec::new(),
            },
        ]
    }

    #[test]
    fn folders_list_prints_one_folder_per_line() {
        let args = ["ts", "folders", "list"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(sample_folders()),
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "/media/movies [ready]\n/media/gone [offline] - not mounted\n"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_list_with_no_folders_prints_nothing() {
        let args = ["ts", "folders", "list"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(Vec::new()),
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn folder_status_labels_match_the_json_contract() {
        for (status, label) in [
            (OwnedFolderStatus::Pending, "pending"),
            (OwnedFolderStatus::Indexing, "indexing"),
            (OwnedFolderStatus::Ready, "ready"),
            (OwnedFolderStatus::Offline, "offline"),
            (OwnedFolderStatus::Error, "error"),
        ] {
            assert_eq!(super::folder_status_label(status), label);
            assert_eq!(serde_json::to_value(status).unwrap(), label);
        }
    }

    #[test]
    fn folders_list_json_prints_a_versioned_document() {
        let args = ["ts", "folders", "list", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(sample_folders()),
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        let folders = document["folders"].as_array().unwrap();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0]["id"], "abc");
        assert_eq!(folders[0]["path"], "/media/movies");
        assert_eq!(folders[0]["status"], "ready");
        assert_eq!(folders[1]["status"], "offline");
        assert_eq!(folders[1]["message"], "not mounted");
        assert!(stderr.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn folders_list_json_with_non_utf8_paths_prints_lossy() {
        use std::os::unix::ffi::OsStringExt;
        // The configured root itself carries invalid UTF-8, as does one
        // recently visited directory beneath it.
        let root = PathBuf::from(OsString::from_vec(b"/media/movies-\xff".to_vec()));
        let visited = PathBuf::from(OsString::from_vec(
            b"/media/movies-\xff/trips-\xfe".to_vec(),
        ));
        let folders = vec![OwnedIndexFolder {
            id: "abc".into(),
            path: root.clone(),
            status: OwnedFolderStatus::Ready,
            message: None,
            scanned_files: 0,
            current_path: None,
            recent_directories: vec![ScannedDirectory {
                path: visited.clone(),
                scanning: false,
                videos: 1,
                images: 0,
                subfolders: 0,
            }],
        }];
        let args = ["ts", "folders", "list", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(folders),
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        let folders = document["folders"].as_array().unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0]["path"], root.to_string_lossy().into_owned());
        assert_eq!(
            folders[0]["recentDirectories"][0]["path"],
            visited.to_string_lossy().into_owned()
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_help_does_not_read_folders() {
        for (arguments, expected) in [
            (
                vec!["ts", "folders", "--help"],
                "Usage: ts folders <command>",
            ),
            (vec!["ts", "folders", "-h"], "Usage: ts folders <command>"),
            (
                vec!["ts", "folders", "list", "--help"],
                super::FOLDERS_LIST_USAGE,
            ),
            (
                vec!["ts", "folders", "list", "-h"],
                super::FOLDERS_LIST_USAGE,
            ),
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{arguments:?}");
            assert!(
                String::from_utf8(stdout).unwrap().contains(expected),
                "{arguments:?}"
            );
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn folders_without_a_known_subcommand_is_a_usage_error() {
        for arguments in [
            vec!["ts", "folders"],
            vec!["ts", "folders", "dance"],
            vec!["ts", "folders", "list", "extra"],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            assert!(!stderr.is_empty());
        }
    }

    #[test]
    fn folders_list_failure_returns_a_failure_status() {
        let args = ["ts", "folders", "list"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Err("no index".to_owned()),
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts folders list: no index\n"
        );
    }

    fn sample_state() -> OwnedIndexState {
        OwnedIndexState {
            supported: true,
            revision: 3,
            indexed_videos: 2,
            indexed_images: 1,
            indexing_files: 0,
            folders: sample_folders(),
        }
    }

    #[test]
    fn folders_add_prints_a_confirmation() {
        let args = ["ts", "folders", "add", "/media/new"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            |path| {
                assert_eq!(path, Path::new("/media/new"));
                Ok((sample_state(), PathBuf::from("/media/new")))
            },
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(String::from_utf8(stdout).unwrap(), "Added /media/new\n");
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_add_reports_the_committed_path() {
        // The manager may store a spelling that differs from what was
        // typed (a symlink retargeted mid-add): the echo names the
        // committed row, never a resolution of the request.
        let args = ["ts", "folders", "add", "/media/a"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            |path| {
                assert_eq!(path, Path::new("/media/a"));
                Ok((sample_state(), PathBuf::from("/media/b")))
            },
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(String::from_utf8(stdout).unwrap(), "Added /media/b\n");
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_add_json_prints_the_resulting_state() {
        let args = ["ts", "folders", "add", "/media/new", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            |_| Ok((sample_state(), PathBuf::from("/media/new"))),
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["revision"], 3);
        assert_eq!(document["folders"].as_array().unwrap().len(), 2);
        assert_eq!(document["folders"][0]["id"], "abc");
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_add_without_a_path_is_a_usage_error() {
        for arguments in [
            vec!["ts", "folders", "add"],
            vec!["ts", "folders", "add", "--json"],
            vec!["ts", "folders", "add", "/media/a", "/media/b"],
            vec!["ts", "folders", "add", "--sort", "/media/a"],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            assert!(
                String::from_utf8(stderr)
                    .unwrap()
                    .contains(super::FOLDERS_ADD_USAGE),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn folders_add_failure_returns_a_failure_status() {
        let args = ["ts", "folders", "add", "/media/new"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            |_| Err("That folder is already covered by a Toka search folder.".to_owned()),
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts folders add: That folder is already covered by a Toka search folder.\n"
        );
    }

    #[test]
    fn folders_remove_prints_a_confirmation() {
        let args = ["ts", "folders", "remove", "/media/movies"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(sample_folders()),
            no_add_folder,
            |id| {
                assert_eq!(id, "abc");
                Ok(sample_state())
            },
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "Removed /media/movies\n"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_remove_matches_the_canonical_path() {
        let directory = tempdir().unwrap();
        let canonical = directory.path().canonicalize().unwrap();
        fs::create_dir(directory.path().join("sub")).unwrap();
        let unclean = directory.path().join("sub").join("..");
        let folders = vec![OwnedIndexFolder {
            id: "abc".into(),
            path: canonical.clone(),
            status: OwnedFolderStatus::Ready,
            message: None,
            scanned_files: 0,
            current_path: None,
            recent_directories: Vec::new(),
        }];
        let args = ["ts", "folders", "remove"]
            .map(OsString::from)
            .into_iter()
            .chain([unclean.into_os_string()])
            .collect::<Vec<_>>();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(folders),
            no_add_folder,
            |id| {
                assert_eq!(id, "abc");
                Ok(sample_state())
            },
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!("Removed {}\n", canonical.display())
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_remove_matches_a_missing_folder_by_its_recorded_path() {
        // /media/gone is not on disk: removing an offline folder by the
        // path it was added under must still find it.
        let args = ["ts", "folders", "remove", "/media/gone"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(sample_folders()),
            no_add_folder,
            |id| {
                assert_eq!(id, "def");
                Ok(sample_state())
            },
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(String::from_utf8(stdout).unwrap(), "Removed /media/gone\n");
        assert!(stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn folders_remove_prefers_the_normalized_match_over_the_spelling_as_typed() {
        // Requested through a symlink: one recorded row names the link
        // spelling, another the folder on disk. The on-disk folder wins
        // even though the literal row is listed first.
        let directory = tempdir().unwrap();
        let target = directory.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let canonical = target.canonicalize().unwrap();
        let folders = ["literal", "canonical"]
            .iter()
            .zip([link.clone(), canonical.clone()])
            .map(|(id, path)| OwnedIndexFolder {
                id: id.to_string(),
                path,
                status: OwnedFolderStatus::Ready,
                message: None,
                scanned_files: 0,
                current_path: None,
                recent_directories: Vec::new(),
            })
            .collect::<Vec<_>>();
        let args = ["ts", "folders", "remove"]
            .map(OsString::from)
            .into_iter()
            .chain([link.into_os_string()])
            .collect::<Vec<_>>();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(folders),
            no_add_folder,
            |id| {
                assert_eq!(id, "canonical");
                Ok(sample_state())
            },
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!("Removed {}\n", canonical.display())
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_remove_of_an_unknown_folder_is_an_error() {
        let args = ["ts", "folders", "remove", "/media/elsewhere"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(sample_folders()),
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts folders remove: /media/elsewhere is not one of Toka's search folders.\n"
        );
    }

    #[test]
    fn folders_remove_json_prints_the_resulting_state() {
        let args = ["ts", "folders", "remove", "/media/movies", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(sample_folders()),
            no_add_folder,
            |_| Ok(sample_state()),
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["folders"].as_array().unwrap().len(), 2);
        assert!(stderr.is_empty());
    }

    #[test]
    fn folders_remove_without_a_path_is_a_usage_error() {
        for arguments in [
            vec!["ts", "folders", "remove"],
            vec!["ts", "folders", "remove", "--json"],
            vec!["ts", "folders", "remove", "/media/a", "/media/b"],
            vec!["ts", "folders", "remove", "--sort", "/media/a"],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            assert!(
                String::from_utf8(stderr)
                    .unwrap()
                    .contains(super::FOLDERS_REMOVE_USAGE),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn folders_remove_failure_returns_a_failure_status() {
        let args = ["ts", "folders", "remove", "/media/movies"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Ok(sample_folders()),
            no_add_folder,
            |_| Err("That search folder is no longer configured.".to_owned()),
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts folders remove: That search folder is no longer configured.\n"
        );
    }

    #[test]
    fn folders_remove_when_folders_cannot_be_read_is_an_error() {
        let args = ["ts", "folders", "remove", "/media/movies"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            || Err("no index".to_owned()),
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts folders remove: no index\n"
        );
    }

    #[test]
    fn folders_remove_help_does_not_remove() {
        for flag in ["--help", "-h"] {
            let args = ["ts", "folders", "remove", flag]
                .map(OsString::from)
                .to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            assert!(
                String::from_utf8(stdout)
                    .unwrap()
                    .contains(super::FOLDERS_REMOVE_USAGE),
                "{flag}"
            );
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn folders_add_help_does_not_add() {
        for flag in ["--help", "-h"] {
            let args = ["ts", "folders", "add", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            assert!(
                String::from_utf8(stdout)
                    .unwrap()
                    .contains(super::FOLDERS_ADD_USAGE),
                "{flag}"
            );
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn sync_path_succeeds_silently() {
        let args = ["ts", "sync-path", "/media/movies/clip.mp4"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            |path| {
                assert_eq!(path, Path::new("/media/movies/clip.mp4"));
                Ok(())
            },
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn sync_path_without_exactly_one_path_is_a_usage_error() {
        for arguments in [
            vec!["ts", "sync-path"],
            vec!["ts", "sync-path", "/media/a", "/media/b"],
            vec!["ts", "sync-path", "--json", "/media/a"],
            vec!["ts", "sync-path", "--sort"],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            assert!(
                String::from_utf8(stderr)
                    .unwrap()
                    .contains(super::SYNC_PATH_USAGE),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn sync_path_failure_returns_a_failure_status() {
        let args = ["ts", "sync-path", "/media/movies/clip.mp4"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            |_| Err("Toka could not use its search index: bad database".to_owned()),
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts sync-path: Toka could not use its search index: bad database\n"
        );
    }

    #[test]
    fn sync_path_help_does_not_sync() {
        for flag in ["--help", "-h"] {
            let args = ["ts", "sync-path", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            assert!(
                String::from_utf8(stdout)
                    .unwrap()
                    .contains(super::SYNC_PATH_USAGE),
                "{flag}"
            );
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn sync_rename_succeeds_silently() {
        let args = ["ts", "sync-rename", "/media/old.mp4", "/media/new.mp4"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            |from, to| {
                assert_eq!(from, Path::new("/media/old.mp4"));
                assert_eq!(to, Path::new("/media/new.mp4"));
                Ok(())
            },
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert!(stdout.is_empty());
        assert!(stderr.is_empty());
    }

    #[test]
    fn sync_rename_without_two_paths_is_a_usage_error() {
        for arguments in [
            vec!["ts", "sync-rename"],
            vec!["ts", "sync-rename", "/media/old.mp4"],
            vec![
                "ts",
                "sync-rename",
                "/media/old.mp4",
                "/media/new.mp4",
                "/media/extra.mp4",
            ],
            vec!["ts", "sync-rename", "--json", "/media/a", "/media/b"],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            assert!(
                String::from_utf8(stderr)
                    .unwrap()
                    .contains(super::SYNC_RENAME_USAGE),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn sync_rename_with_an_empty_path_is_a_usage_error() {
        for arguments in [
            vec!["ts", "sync-rename", "", "/media/new.mp4"],
            vec!["ts", "sync-rename", "/media/old.mp4", ""],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            let stderr = String::from_utf8(stderr).unwrap();
            assert!(
                stderr.contains("A path must not be empty."),
                "{arguments:?}"
            );
            assert!(stderr.contains(super::SYNC_RENAME_USAGE), "{arguments:?}");
        }
    }

    #[test]
    fn sync_rename_failure_returns_a_failure_status() {
        let args = ["ts", "sync-rename", "/media/old.mp4", "/media/new.mp4"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            |_, _| Err("Toka could not use its search index: bad database".to_owned()),
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts sync-rename: Toka could not use its search index: bad database\n"
        );
    }

    #[test]
    fn sync_rename_help_does_not_sync() {
        for flag in ["--help", "-h"] {
            let args = ["ts", "sync-rename", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            assert!(
                String::from_utf8(stdout)
                    .unwrap()
                    .contains(super::SYNC_RENAME_USAGE),
                "{flag}"
            );
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn thumbnail_prints_the_thumbnail_path() {
        let args = ["ts", "thumbnail", "/media/movies/clip.mp4"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            |path| {
                assert_eq!(path, Path::new("/media/movies/clip.mp4"));
                Ok(PathBuf::from("/tmp/toka-thumbnails/abc123.jpg"))
            },
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "/tmp/toka-thumbnails/abc123.jpg\n"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn thumbnail_json_prints_a_versioned_document() {
        let args = ["ts", "thumbnail", "/media/movies/clip.mp4", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            |_| Ok(PathBuf::from("/tmp/toka-thumbnails/abc123.jpg")),
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(document["thumbnail"], "/tmp/toka-thumbnails/abc123.jpg");
        assert!(stderr.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn thumbnail_json_with_non_utf8_path_prints_lossy() {
        use std::os::unix::ffi::OsStringExt;
        let generated = PathBuf::from(OsString::from_vec(
            b"/tmp/toka-thumbnails/abc-\xff.jpg".to_vec(),
        ));
        let args = ["ts", "thumbnail", "/media/movies/clip.mp4", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            |_| Ok(generated.clone()),
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(
            document["thumbnail"],
            generated.to_string_lossy().into_owned()
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn thumbnail_without_a_path_is_a_usage_error() {
        for arguments in [
            vec!["ts", "thumbnail"],
            vec!["ts", "thumbnail", "--json"],
            vec!["ts", "thumbnail", "/media/a", "/media/b"],
            vec!["ts", "thumbnail", "--sort", "/media/a"],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            assert!(
                String::from_utf8(stderr)
                    .unwrap()
                    .contains(super::THUMBNAIL_USAGE),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn thumbnail_failure_returns_a_failure_status() {
        let args = ["ts", "thumbnail", "/media/movies/clip.mp4"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            |_| Err("Toka could not make a thumbnail for /media/movies/clip.mp4.".to_owned()),
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts thumbnail: Toka could not make a thumbnail for /media/movies/clip.mp4.\n"
        );
    }

    #[test]
    fn thumbnail_help_does_not_generate() {
        for flag in ["--help", "-h"] {
            let args = ["ts", "thumbnail", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            assert!(
                String::from_utf8(stdout)
                    .unwrap()
                    .contains(super::THUMBNAIL_USAGE),
                "{flag}"
            );
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn preview_prints_one_frame_per_line() {
        let args = ["ts", "preview", "/media/movies/clip.mp4"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            |path| {
                assert_eq!(path, Path::new("/media/movies/clip.mp4"));
                Ok(vec![
                    PathBuf::from("/tmp/toka-thumbnails/abc-preview-0.jpg"),
                    PathBuf::from("/tmp/toka-thumbnails/abc-preview-1.jpg"),
                ])
            },
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "/tmp/toka-thumbnails/abc-preview-0.jpg\n/tmp/toka-thumbnails/abc-preview-1.jpg\n"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn preview_json_prints_a_versioned_document() {
        let args = ["ts", "preview", "/media/movies/clip.mp4", "--json"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            |_| {
                Ok(vec![
                    PathBuf::from("/tmp/toka-thumbnails/abc-preview-0.jpg"),
                    PathBuf::from("/tmp/toka-thumbnails/abc-preview-1.jpg"),
                ])
            },
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        let document: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(document["schema"], 1);
        assert_eq!(
            document["previews"],
            serde_json::json!([
                "/tmp/toka-thumbnails/abc-preview-0.jpg",
                "/tmp/toka-thumbnails/abc-preview-1.jpg",
            ])
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn preview_without_a_path_is_a_usage_error() {
        for arguments in [
            vec!["ts", "preview"],
            vec!["ts", "preview", "--json"],
            vec!["ts", "preview", "/media/a", "/media/b"],
            vec!["ts", "preview", "--sort", "/media/a"],
        ] {
            let args = arguments.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2, "{arguments:?}");
            assert!(stdout.is_empty());
            assert!(
                String::from_utf8(stderr)
                    .unwrap()
                    .contains(super::PREVIEW_USAGE),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn preview_with_an_empty_path_is_a_usage_error() {
        let args = ["ts", "preview", ""].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains("A path must not be empty."));
        assert!(stderr.contains(super::PREVIEW_USAGE));
    }

    #[test]
    fn preview_failure_returns_a_failure_status() {
        let args = ["ts", "preview", "/media/movies/clip.mp4"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            |_| Err("Toka could not make a preview for /media/movies/clip.mp4.".to_owned()),
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts preview: Toka could not make a preview for /media/movies/clip.mp4.\n"
        );
    }

    #[test]
    fn preview_help_does_not_generate() {
        for flag in ["--help", "-h"] {
            let args = ["ts", "preview", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            assert!(
                String::from_utf8(stdout)
                    .unwrap()
                    .contains(super::PREVIEW_USAGE),
                "{flag}"
            );
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn bare_and_unknown_commands_print_help_to_stderr() {
        for args in [
            vec![OsString::from("ts")],
            ["ts", "dance"].map(OsString::from).to_vec(),
        ] {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 2);
            assert!(stdout.is_empty());
            assert!(String::from_utf8(stderr)
                .unwrap()
                .contains("Usage: ts <command>"));
        }
    }

    #[test]
    fn version_flags_print_without_building_anything() {
        for flag in ["-v", "--version"] {
            let args = ["ts", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            let output = String::from_utf8(stdout).unwrap();
            let prefix = format!("ts {} (Git SHA ", super::build_version());
            let sha = output
                .strip_prefix(&prefix)
                .and_then(|rest| rest.strip_suffix(")\n"))
                .unwrap_or_else(|| panic!("unexpected version line: {output}"));
            assert_eq!(
                Some(sha),
                option_env!("TOKA_GIT_SHA").or(Some("development"))
            );
            assert!(stderr.is_empty());
        }
    }

    fn running_daemon() -> DaemonStatus {
        DaemonStatus {
            running: true,
            pid: Some(1234),
            uptime_secs: Some(65),
            heartbeat_age_secs: Some(1),
            activity: Some(DaemonActivity::Scanning),
            current_root: Some(PathBuf::from("/media")),
            current_path: Some(PathBuf::from("/media/a.mp4")),
            queue: DaemonQueue {
                pending_roots: 1,
                files_remaining: 12,
                pending_repairs: 0,
                pending_thumbnails: 0,
            },
            offline_roots: 0,
            service_running: Some(true),
        }
    }

    fn stopped_daemon() -> DaemonStatus {
        DaemonStatus {
            running: false,
            pid: Some(1234),
            uptime_secs: Some(90),
            heartbeat_age_secs: Some(30),
            activity: None,
            current_root: None,
            current_path: None,
            queue: DaemonQueue::default(),
            offline_roots: 0,
            service_running: Some(false),
        }
    }

    fn idle_daemon() -> DaemonStatus {
        DaemonStatus {
            running: true,
            pid: Some(1234),
            uptime_secs: Some(65),
            heartbeat_age_secs: Some(1),
            activity: Some(DaemonActivity::Idle),
            current_root: None,
            current_path: None,
            queue: DaemonQueue::default(),
            offline_roots: 0,
            service_running: None,
        }
    }

    #[test]
    fn status_help_prints_without_reading_anything() {
        for flag in ["-h", "--help"] {
            let args = ["ts", "status", flag].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{flag}");
            let output = String::from_utf8(stdout).unwrap();
            assert!(output.contains(super::STATUS_USAGE), "{output}");
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn status_reports_a_running_daemon_and_succeeds() {
        let args = ["ts", "status"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            || Ok(running_daemon()),
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("Daemon:   running"), "{output}");
        assert!(output.contains("Activity: scanning"), "{output}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn status_reports_a_stopped_daemon_with_exit_3() {
        let args = ["ts", "status"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            || Ok(stopped_daemon()),
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 3);
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("Daemon:   not running"), "{output}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn status_json_is_a_versioned_document_and_keeps_exit_3() {
        for stopped in [false, true] {
            let args = ["ts", "status", "--json"].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let snapshot = if stopped {
                stopped_daemon()
            } else {
                running_daemon()
            };

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                || Ok(snapshot.clone()),
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, if stopped { 3 } else { 0 });
            let output = String::from_utf8(stdout).unwrap();
            let document: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(document["schema"], 1);
            assert_eq!(document["running"], !stopped);
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn status_rejects_unknown_options_with_usage() {
        let args = ["ts", "status", "--verbose"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("{}\n", super::STATUS_USAGE)
        );
    }

    #[test]
    fn status_read_failure_returns_a_failure_status() {
        let args = ["ts", "status"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            || Err::<DaemonStatus, _>("the index is locked".to_owned()),
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts status: the index is locked\n"
        );
    }

    #[test]
    fn status_watch_on_an_idle_daemon_renders_once_without_waiting() {
        let args = ["ts", "status", "--watch"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            || Ok(idle_daemon()),
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        assert_eq!(output.matches("Activity: idle").count(), 1, "{output}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn status_watch_on_a_stopped_daemon_exits_3_without_waiting() {
        let args = ["ts", "status", "--watch"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            || Ok(stopped_daemon()),
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 3);
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("Daemon:   not running"), "{output}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn watch_rerenders_until_idle_and_waits_between() {
        let mut reads = [running_daemon(), running_daemon(), idle_daemon()].into_iter();
        let waits = std::cell::Cell::new(0);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || Ok(reads.next().unwrap()),
            false,
            true,
            || waits.set(waits.get() + 1),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert_eq!(waits.get(), 2);
        let output = String::from_utf8(stdout).unwrap();
        assert_eq!(output.matches("Activity: scanning").count(), 2, "{output}");
        assert_eq!(output.matches("Activity: idle").count(), 1, "{output}");
        // The first render prints plainly; later ones clear the screen.
        assert!(!output.starts_with("\x1b[2J\x1b[H"), "{output:?}");
        assert_eq!(output.matches("\x1b[2J\x1b[H").count(), 2, "{output:?}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn watch_streams_json_documents_without_clearing() {
        let mut reads = [running_daemon(), idle_daemon()].into_iter();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || Ok(reads.next().unwrap()),
            true,
            false,
            || {},
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        let output = String::from_utf8(stdout).unwrap();
        assert!(!output.contains('\x1b'), "{output:?}");
        // One compact document per line: each line parses on its own.
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2, "{output}");
        for line in lines {
            let document: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(document["schema"], 1, "{line}");
            assert_eq!(document["running"], true, "{line}");
        }
        assert!(stderr.is_empty());
    }

    #[test]
    fn watch_stops_with_exit_3_when_the_daemon_stops() {
        let mut reads = [running_daemon(), stopped_daemon()].into_iter();
        let waits = std::cell::Cell::new(0);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || Ok(reads.next().unwrap()),
            false,
            true,
            || waits.set(waits.get() + 1),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 3);
        assert_eq!(waits.get(), 1);
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("Activity: scanning"), "{output}");
        assert!(output.contains("Daemon:   not running"), "{output}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn watch_waits_while_idle_with_work_queued() {
        // An idle report with folders still pending predates the daemon's
        // reconcile: the watch holds until the queue drains to idle.
        let mut pending = idle_daemon();
        pending.queue.pending_roots = 1;
        let mut reads = [pending, idle_daemon()].into_iter();
        let waits = std::cell::Cell::new(0);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || Ok(reads.next().unwrap()),
            false,
            false,
            || waits.set(waits.get() + 1),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert_eq!(waits.get(), 1);
        let output = String::from_utf8(stdout).unwrap();
        assert_eq!(output.matches("Activity: idle").count(), 2, "{output}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn watch_waits_while_offline_waiting_with_work_queued() {
        // Offline-waiting with queued work is not done: the daemon still
        // has actionable work (an offline-only queue would already read
        // empty), so the watch holds until the queue drains.
        let mut pending = idle_daemon();
        pending.activity = Some(DaemonActivity::OfflineWaiting);
        pending.queue.pending_roots = 1;
        let mut reads = [pending, idle_daemon()].into_iter();
        let waits = std::cell::Cell::new(0);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || Ok(reads.next().unwrap()),
            false,
            false,
            || waits.set(waits.get() + 1),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert_eq!(waits.get(), 1);
        let output = String::from_utf8(stdout).unwrap();
        assert_eq!(
            output.matches("Activity: offline-waiting").count(),
            1,
            "{output}"
        );
        assert_eq!(output.matches("Activity: idle").count(), 1, "{output}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn watch_exits_when_only_offline_folders_remain() {
        // Offline folders wait for their storage, not for the daemon: an
        // offline-waiting daemon with nothing actionable queued is done,
        // and the report says what is still offline.
        let mut waiting = idle_daemon();
        waiting.activity = Some(DaemonActivity::OfflineWaiting);
        waiting.offline_roots = 2;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || Ok(waiting.clone()),
            false,
            false,
            || panic!("an offline-only watch must not wait"),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        let output = String::from_utf8(stdout).unwrap();
        assert_eq!(
            output.matches("Activity: offline-waiting").count(),
            1,
            "{output}"
        );
        assert!(
            output.contains("Offline:  2 folders waiting for storage\n"),
            "{output}"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn watch_exits_on_idle_with_offline_folders_outstanding() {
        // Idle with an empty queue is done even when offline folders are
        // listed: they never hold the wait.
        let mut idle = idle_daemon();
        idle.offline_roots = 1;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || Ok(idle.clone()),
            false,
            false,
            || panic!("an idle watch with only offline folders must not wait"),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("Activity: idle\n"), "{output}");
        assert!(
            output.contains("Offline:  1 folder waiting for storage\n"),
            "{output}"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn watch_keeps_watching_an_unknown_activity() {
        // Only a known-idle daemon proves the queue drained: an activity
        // from a newer daemon holds the watch even with an empty queue.
        let mut unknown = idle_daemon();
        unknown.activity = Some(DaemonActivity::Unknown);
        let mut reads = [unknown, idle_daemon()].into_iter();
        let waits = std::cell::Cell::new(0);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || Ok(reads.next().unwrap()),
            false,
            false,
            || waits.set(waits.get() + 1),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert_eq!(waits.get(), 1);
        let output = String::from_utf8(stdout).unwrap();
        assert_eq!(output.matches("Activity: unknown").count(), 1, "{output}");
        assert_eq!(output.matches("Activity: idle").count(), 1, "{output}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn watch_without_a_terminal_never_clears_the_screen() {
        // Piped watch output is a plain stream of reports: no escape
        // sequences between renders.
        let mut reads = [running_daemon(), running_daemon(), idle_daemon()].into_iter();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || Ok(reads.next().unwrap()),
            false,
            false,
            || {},
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        let output = String::from_utf8(stdout).unwrap();
        assert_eq!(output.matches("Activity: scanning").count(), 2, "{output}");
        assert_eq!(output.matches("Activity: idle").count(), 1, "{output}");
        assert!(!output.contains('\x1b'), "{output:?}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn watch_read_failure_retries_before_failing() {
        // A failure that outlasts the retry bound still exits 1, after one
        // wait per retry and a single report of the last error.
        let reads = std::cell::Cell::new(0u32);
        let waits = std::cell::Cell::new(0u32);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || {
                reads.set(reads.get() + 1);
                Err::<DaemonStatus, _>("the index is locked".to_owned())
            },
            false,
            false,
            || waits.set(waits.get() + 1),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 1);
        assert_eq!(reads.get(), 1 + super::WATCH_READ_RETRIES);
        assert_eq!(waits.get(), super::WATCH_READ_RETRIES);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "ts status: the index is locked\n"
        );
    }

    #[test]
    fn watch_recovers_after_transient_read_failures() {
        // Two failed reads ride out the retry bound silently; the
        // recovered read renders and the watch continues from it.
        let mut reads = [
            Err("the index is locked".to_owned()),
            Err("the index is locked".to_owned()),
            Ok(idle_daemon()),
        ]
        .into_iter();
        let waits = std::cell::Cell::new(0);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = super::run_status_watch(
            || reads.next().unwrap(),
            false,
            false,
            || waits.set(waits.get() + 1),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        assert_eq!(waits.get(), 2);
        let output = String::from_utf8(stdout).unwrap();
        assert_eq!(output.matches("Activity: idle").count(), 1, "{output}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn status_reports_an_unknown_activity_and_succeeds() {
        // A newer daemon's activity renders verbatim; the daemon is
        // running, so the command still exits 0.
        let args = ["ts", "status"].map(OsString::from).to_vec();
        let mut unknown = idle_daemon();
        unknown.activity = Some(DaemonActivity::Unknown);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            || Ok(unknown.clone()),
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        let output = String::from_utf8(stdout).unwrap();
        assert!(output.contains("Activity: unknown\n"), "{output}");
        assert!(stderr.is_empty());
    }

    #[test]
    fn daemon_help_prints_without_running_anything() {
        for args in [
            &["ts", "daemon", "--help"][..],
            &["ts", "daemon", "run", "--help"][..],
        ] {
            let args = args.iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                no_daemon_run,
                &mut stdout,
                &mut stderr,
            );

            assert_eq!(status, 0, "{args:?}");
            assert!(!stdout.is_empty());
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn daemon_without_a_subcommand_prints_usage() {
        let args = ["ts", "daemon"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("{}\n", super::DAEMON_HELP)
        );
    }

    #[test]
    fn daemon_unknown_subcommand_prints_usage() {
        let args = ["ts", "daemon", "stop"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains(super::DAEMON_HELP));
    }

    #[test]
    fn daemon_run_rejects_arguments_with_usage() {
        let args = ["ts", "daemon", "run", "--foreground"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            no_engine,
            no_verbose_engine,
            no_stats,
            no_folders,
            no_add_folder,
            no_remove_folder,
            no_sync_path,
            no_sync_rename,
            no_thumbnail,
            no_preview,
            no_status,
            no_daemon_run,
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            format!("{}\n", super::DAEMON_RUN_USAGE)
        );
    }

    #[test]
    fn daemon_run_reports_the_daemon_result() {
        for result in [
            Ok(()),
            Err("Another Toka indexer daemon is already running.".to_owned()),
        ] {
            let args = ["ts", "daemon", "run"].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(
                &args,
                no_engine,
                no_verbose_engine,
                no_stats,
                no_folders,
                no_add_folder,
                no_remove_folder,
                no_sync_path,
                no_sync_rename,
                no_thumbnail,
                no_preview,
                no_status,
                || result.clone(),
                &mut stdout,
                &mut stderr,
            );

            if result.is_ok() {
                assert_eq!(status, 0);
                assert!(stdout.is_empty());
                assert!(stderr.is_empty());
            } else {
                assert_eq!(status, 1);
                assert!(stdout.is_empty());
                assert_eq!(
                    String::from_utf8(stderr).unwrap(),
                    "ts daemon run: Another Toka indexer daemon is already running.\n"
                );
            }
        }
    }
}
