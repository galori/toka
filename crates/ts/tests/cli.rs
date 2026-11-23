use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn run_ts(arguments: &[&str], videos: &[PathBuf]) -> Output {
    let fixture_videos = videos
        .iter()
        .map(|video| video.as_os_str())
        .collect::<Vec<_>>();
    run_ts_with_fixture_env(arguments, std::env::join_paths(fixture_videos).ok())
}

fn run_ts_with_fixture_env(arguments: &[&str], fixture_videos: Option<OsString>) -> Output {
    let home = tempdir().unwrap();
    let data_home = home.path().join("data");
    fs::create_dir_all(&data_home).unwrap();

    run_ts_with_home(arguments, home.path(), &data_home, fixture_videos, None)
}

fn ts_command(
    arguments: &[&str],
    home: &Path,
    data_home: &Path,
    fixture_videos: Option<OsString>,
    cwd: Option<&Path>,
) -> Command {
    ts_command_for_binary(
        Path::new(env!("CARGO_BIN_EXE_ts")),
        arguments,
        home,
        data_home,
        fixture_videos,
        cwd,
    )
}

/// [`ts_command`] running a staged copy of the binary instead of the test
/// target itself, so installs from metacharacter paths are testable.
fn ts_command_for_binary(
    binary: &Path,
    arguments: &[&str],
    home: &Path,
    data_home: &Path,
    fixture_videos: Option<OsString>,
    cwd: Option<&Path>,
) -> Command {
    // A scratch temp space too, so even cache reads cannot escape the
    // test's directories.
    let scratch = home.join("tmp");
    fs::create_dir_all(&scratch).unwrap();

    let mut command = Command::new(binary);
    command
        .args(arguments)
        .env("HOME", home)
        .env("XDG_DATA_HOME", data_home)
        .env("TMPDIR", &scratch)
        // The daemon lock follows the runtime dir, so point it at scratch
        // too: no test may lock (or create files) in the real one.
        .env("TOKA_RUNTIME_DIR", scratch.join("runtime"));
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    match fixture_videos {
        Some(videos) => {
            command.env("TS_FIXTURE_VIDEOS", videos);
        }
        None => {
            command.env_remove("TS_FIXTURE_VIDEOS");
        }
    }
    command
}

fn run_ts_with_home(
    arguments: &[&str],
    home: &Path,
    data_home: &Path,
    fixture_videos: Option<OsString>,
    cwd: Option<&Path>,
) -> Output {
    let child = ts_command(arguments, home, data_home, fixture_videos, cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_exit(child)
}

/// Starts `ts daemon run` against a scratch home and returns the live
/// child. The caller must kill it: tests wrap their body so a failure
/// cannot orphan a watcher on the test directories.
fn spawn_daemon(home: &ScratchHome) -> Child {
    ts_command(
        &["daemon", "run"],
        home.home.path(),
        &home.data_home,
        None,
        None,
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap()
}

/// Runs a daemon test body against a live daemon, killing the daemon
/// afterwards even when the body panics, then re-raising the failure.
fn with_daemon(home: &ScratchHome, body: impl FnOnce() + std::panic::UnwindSafe) {
    let mut daemon = spawn_daemon(home);
    let result = std::panic::catch_unwind(body);
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

/// Polls `ts status --json` until `ready` accepts it or the timeout
/// lapses. A stopped daemon still prints a document (with exit 3), so
/// every poll either parses or fails the wait, never the test.
fn wait_for_status(
    home: &ScratchHome,
    timeout: Duration,
    mut ready: impl FnMut(&serde_json::Value) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let output = run_at(home, &["status", "--json"]);
        if let Ok(document) =
            serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&output.stdout))
        {
            if ready(&document) {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for_exit(child: Child) -> Output {
    wait_for_exit_timeout(child, Duration::from_secs(10))
}

fn wait_for_exit_timeout(mut child: Child, timeout: Duration) -> Output {
    let started = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "ts did not exit in time; stdout: {}; stderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn create_video(directory: &Path, number: usize) -> PathBuf {
    let path = directory.join(format!("family-trip-{number:02}.mp4"));
    fs::write(&path, b"fixture video").unwrap();
    path
}

#[cfg(feature = "fixtures")]
#[test]
fn search_prints_every_matching_path_and_exits_successfully() {
    let directory = tempdir().unwrap();
    let mut videos = (0..25)
        .map(|number| create_video(directory.path(), number))
        .collect::<Vec<_>>();
    let image = directory.path().join("family-trip-image.jpg");
    fs::write(&image, b"fixture image").unwrap();
    videos.push(image);
    videos.sort();
    let expected = videos
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";

    let output = run_ts(&["search", "family", "trip"], &videos);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    assert!(output.stderr.is_empty());
}

#[test]
fn search_without_a_query_prints_usage_and_exits_with_a_usage_error() {
    let output = run_ts(&["search"], &[]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Search needs a query.\nUsage: ts search <query> [--json] [--limit N] [--page N] [--type video|image|all] [--count] [--verbose]\n"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn search_help_prints_usage_and_exits_successfully() {
    let output = run_ts(&["search", "--help"], &[]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Usage: ts search <query> [--json] [--limit N] [--page N] [--type video|image|all] [--count] [--verbose]\n\nSearch Toka's indexed videos and images. Print one full path per result line, sorted by path so pages stay stable across runs.\n\n  --json       Print one JSON document (schema 1) instead of paths\n  --limit N    Print at most N results (applied after --page)\n  --page N     Print page N of the results (24 per page)\n  --type T     Only video, image, or all results (default all)\n  --count      Print only how many results matched\n  --verbose    Stream the per-search trace to stderr\n\nUse -- to search for text starting with a dash.\n"
    );
    assert!(output.stderr.is_empty());
}

#[cfg(feature = "fixtures")]
#[test]
fn search_with_no_matches_succeeds_without_output() {
    let output = run_ts(&["search", "missing"], &[]);

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[cfg(feature = "fixtures")]
#[test]
fn search_without_fixture_videos_configured_fails() {
    let output = run_ts_with_fixture_env(&["search", "family"], None);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "ts search: The integration-test videos were not configured.\n"
    );
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn search_ignores_fixture_videos_without_the_fixtures_feature() {
    let directory = tempdir().unwrap();
    let video = create_video(directory.path(), 0);

    let output = run_ts(&["search", "family"], &[video]);

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[cfg(feature = "fixtures")]
#[test]
fn search_json_prints_a_versioned_document() {
    let directory = tempdir().unwrap();
    let videos = (0..2)
        .map(|number| create_video(directory.path(), number))
        .collect::<Vec<_>>();

    let output = run_ts(&["search", "family", "--json"], &videos);

    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["query"], "family");
    assert_eq!(document["totalResults"], 2);
    assert_eq!(document["results"].as_array().unwrap().len(), 2);
    assert!(output.stderr.is_empty());
}

#[cfg(feature = "fixtures")]
#[test]
fn search_verbose_streams_the_trace_to_stderr() {
    let directory = tempdir().unwrap();
    let matched = create_video(directory.path(), 0);
    let skipped = directory.path().join("unrelated.mp4");
    fs::write(&skipped, b"fixture video").unwrap();

    let output = run_ts(
        &["search", "family", "--verbose"],
        &[matched.clone(), skipped.clone()],
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", matched.display())
    );
    let trace = String::from_utf8(output.stderr).unwrap();
    assert!(
        trace.contains(&format!("FILTERED OUT: {}", skipped.display())),
        "{trace}"
    );
    assert!(trace.contains("RESULTS RETURNED TO APP:"), "{trace}");
}

#[test]
fn stats_with_no_index_reports_empty() {
    let output = run_ts(&["stats"], &[]);

    assert_eq!(output.status.code(), Some(0));
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.contains("Videos:   0"), "{report}");
    assert!(output.stderr.is_empty());
}

#[test]
fn stats_json_with_no_index_is_an_empty_document() {
    let output = run_ts(&["stats", "--json"], &[]);

    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["videos"], 0);
    assert_eq!(document["folders"].as_array().unwrap().len(), 0);
    assert!(output.stderr.is_empty());
}

#[test]
fn folders_list_with_no_index_is_empty() {
    let output = run_ts(&["folders", "list"], &[]);

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn folders_list_json_with_no_index_is_an_empty_document() {
    let output = run_ts(&["folders", "list", "--json"], &[]);

    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["folders"].as_array().unwrap().len(), 0);
    assert!(output.stderr.is_empty());
}

/// Where the child resolves the index: dirs honors XDG_DATA_HOME on
/// Linux but derives it from HOME on macOS.
fn index_database_path(home: &Path, data_home: &Path) -> PathBuf {
    // Each platform resolves from one root and ignores the other.
    #[cfg(target_os = "macos")]
    let _ = data_home;
    #[cfg(not(target_os = "macos"))]
    let _ = home;
    #[cfg(target_os = "macos")]
    let database = home
        .join("Library/Application Support")
        .join(toka_search::config::APP_DATA_DIR_NAME)
        .join(toka_search::config::INDEX_FILE_NAME);
    #[cfg(not(target_os = "macos"))]
    let database = data_home
        .join(toka_search::config::APP_DATA_DIR_NAME)
        .join(toka_search::config::INDEX_FILE_NAME);
    database
}

/// Runs `ts` with explicit HOME / data-home / TMPDIR, creating none of
/// them: the caller sets up exactly the layout the probe needs —
/// including layouts that cannot exist, like an overlong path.
fn run_ts_with_raw_envs(
    arguments: &[&str],
    home: &Path,
    data_home: &Path,
    tmpdir: &Path,
) -> Output {
    let child = Command::new(env!("CARGO_BIN_EXE_ts"))
        .args(arguments)
        .env("HOME", home)
        .env("XDG_DATA_HOME", data_home)
        .env("TMPDIR", tmpdir)
        .env_remove("TS_FIXTURE_VIDEOS")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_exit(child)
}

/// A scratch layout whose index path fails metadata with ENAMETOOLONG on
/// Unix for every user: a component past NAME_MAX under both HOME (what
/// macOS resolves) and XDG_DATA_HOME (what Linux resolves), with a
/// normal TMPDIR beside them. Nothing under the overlong components is
/// ever created — metadata fails whether or not the path exists.
fn overlong_env() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let base = tempdir().unwrap();
    let overlong = "a".repeat(300);
    let home = base.path().join(&overlong).join("home");
    let data_home = base.path().join(&overlong).join("data");
    let tmpdir = base.path().join("tmp");
    fs::create_dir_all(&tmpdir).unwrap();
    (base, home, data_home, tmpdir)
}

#[test]
fn stats_with_an_unreadable_index_errors_instead_of_reporting_empty() {
    let (_base, home, data_home, tmpdir) = overlong_env();

    for arguments in [vec!["stats"], vec!["stats", "--json"]] {
        let output = run_ts_with_raw_envs(&arguments, &home, &data_home, &tmpdir);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("could not read its search index"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn folders_list_with_an_unreadable_index_errors_instead_of_reporting_empty() {
    let (_base, home, data_home, tmpdir) = overlong_env();

    for arguments in [vec!["folders", "list"], vec!["folders", "list", "--json"]] {
        let output = run_ts_with_raw_envs(&arguments, &home, &data_home, &tmpdir);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("could not read its search index"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn search_with_an_unreadable_index_errors_instead_of_searching_empty() {
    let (_base, home, data_home, tmpdir) = overlong_env();

    for arguments in [vec!["search", "family"], vec!["search", "family", "--json"]] {
        let output = run_ts_with_raw_envs(&arguments, &home, &data_home, &tmpdir);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("could not read its search index"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn stats_with_a_directory_at_the_index_path_errors_instead_of_reporting_empty() {
    let home = scratch_home();
    let database = index_database_path(home.home.path(), &home.data_home);
    fs::create_dir_all(&database).unwrap();

    for arguments in [vec!["stats"], vec!["stats", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("is not a file"), "{arguments:?}: {stderr}");
    }
}

#[test]
fn folders_list_with_a_directory_at_the_index_path_errors_instead_of_reporting_empty() {
    let home = scratch_home();
    let database = index_database_path(home.home.path(), &home.data_home);
    fs::create_dir_all(&database).unwrap();

    for arguments in [vec!["folders", "list"], vec!["folders", "list", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("is not a file"), "{arguments:?}: {stderr}");
    }
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn search_with_a_directory_at_the_index_path_errors_instead_of_searching_empty() {
    let home = scratch_home();
    let database = index_database_path(home.home.path(), &home.data_home);
    fs::create_dir_all(&database).unwrap();

    for arguments in [vec!["search", "family"], vec!["search", "family", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("is not a file"), "{arguments:?}: {stderr}");
    }
}

/// Writes a version-999 database at the child's index path: an index
/// from a newer Toka that this one must refuse, not migrate.
fn write_newer_schema_database(database: &Path) {
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    toka_search::owned_index::OwnedIndex::open(database).unwrap();
    rusqlite::Connection::open(database)
        .unwrap()
        .pragma_update(None, "user_version", 999)
        .unwrap();
}

fn write_corrupt_database(database: &Path) {
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(database, b"this is not a SQLite database at all").unwrap();
}

#[test]
fn stats_on_a_newer_schema_errors_instead_of_reporting() {
    let home = scratch_home();
    write_newer_schema_database(&index_database_path(home.home.path(), &home.data_home));

    for arguments in [vec!["stats"], vec!["stats", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("schema version 999"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn folders_list_on_a_newer_schema_errors_instead_of_listing() {
    let home = scratch_home();
    write_newer_schema_database(&index_database_path(home.home.path(), &home.data_home));

    for arguments in [vec!["folders", "list"], vec!["folders", "list", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("schema version 999"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn search_on_a_newer_schema_errors_instead_of_searching() {
    let home = scratch_home();
    write_newer_schema_database(&index_database_path(home.home.path(), &home.data_home));

    for arguments in [vec!["search", "family"], vec!["search", "family", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("schema version 999"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn folders_list_on_a_corrupt_index_errors_instead_of_listing() {
    let home = scratch_home();
    write_corrupt_database(&index_database_path(home.home.path(), &home.data_home));

    for arguments in [vec!["folders", "list"], vec!["folders", "list", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("could not use its search index"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn stats_on_a_corrupt_index_errors_instead_of_reporting() {
    let home = scratch_home();
    write_corrupt_database(&index_database_path(home.home.path(), &home.data_home));

    for arguments in [vec!["stats"], vec!["stats", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("could not use its search index"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn search_on_a_corrupt_index_errors_instead_of_searching() {
    let home = scratch_home();
    write_corrupt_database(&index_database_path(home.home.path(), &home.data_home));

    for arguments in [vec!["search", "family"], vec!["search", "family", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("could not use its search index"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[cfg(feature = "fixtures")]
#[test]
fn search_page_past_the_last_page_succeeds_without_output() {
    let directory = tempdir().unwrap();
    let videos = (0..2)
        .map(|number| create_video(directory.path(), number))
        .collect::<Vec<_>>();

    let output = run_ts(&["search", "family", "--page", "5"], &videos);

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn real_index_search_stats_and_folders_agree() {
    use toka_search::owned_index::{OwnedIndex, OwnedIndexManager};

    let home = tempdir().unwrap();
    let data_home = home.path().join("data");
    fs::create_dir_all(&data_home).unwrap();
    // Where the child resolves the index: dirs honors XDG_DATA_HOME on
    // Linux but derives it from HOME on macOS.
    #[cfg(target_os = "macos")]
    let database = home
        .path()
        .join("Library/Application Support")
        .join(toka_search::config::APP_DATA_DIR_NAME)
        .join(toka_search::config::INDEX_FILE_NAME);
    #[cfg(not(target_os = "macos"))]
    let database = data_home
        .join(toka_search::config::APP_DATA_DIR_NAME)
        .join(toka_search::config::INDEX_FILE_NAME);

    let media = home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    // The index stores canonical paths; the test compares against them.
    let media = media.canonicalize().unwrap();
    let first = create_video(&media, 0);
    let second = create_video(&media, 1);

    let manager = OwnedIndexManager::under(database.clone());
    let state = manager.add_folder(&media).unwrap();
    let root = state.folders[0].path.clone();
    OwnedIndex::open(&database)
        .unwrap()
        .scan_root(&state.folders[0].id, &root, |_| {})
        .unwrap();

    let output = run_ts_with_home(&["search", "family"], home.path(), &data_home, None, None);
    assert_eq!(output.status.code(), Some(0));
    let mut expected = [first.display().to_string(), second.display().to_string()];
    expected.sort();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        expected.join("\n") + "\n"
    );
    assert!(output.stderr.is_empty());

    let output = run_ts_with_home(&["stats", "--json"], home.path(), &data_home, None, None);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["videos"], 2);
    assert_eq!(document["folders"].as_array().unwrap().len(), 1);
    assert!(output.stderr.is_empty());

    let output = run_ts_with_home(&["folders", "list"], home.path(), &data_home, None, None);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{} [ready]\n", root.display())
    );
    assert!(output.stderr.is_empty());
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn read_only_commands_create_no_state_without_an_index() {
    let home = tempdir().unwrap();
    let data_home = home.path().join("data");
    fs::create_dir_all(&data_home).unwrap();

    for arguments in [
        vec!["search", "family"],
        vec!["search", "family", "--count"],
        vec!["search", "family", "--count", "--json"],
        vec!["search", "family", "--json"],
        vec!["search", "family", "--verbose"],
        vec!["search", "family", "--type", "video"],
        vec!["search", "family", "--type", "image"],
        vec!["stats"],
        vec!["stats", "--json"],
        vec!["folders", "list"],
        vec!["folders", "list", "--json"],
    ] {
        let output = run_ts_with_home(&arguments, home.path(), &data_home, None, None);
        assert_eq!(output.status.code(), Some(0), "{arguments:?}");
        // Exit 0 alone would let an empty engine print garbage; the empty
        // outputs are part of the no-create contract.
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        if arguments.contains(&"--json") {
            let document: serde_json::Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(document["schema"], 1, "{arguments:?}");
            assert!(stderr.is_empty(), "{arguments:?}");
            if arguments.contains(&"--count") {
                assert_eq!(document["totalResults"], 0, "{arguments:?}");
            } else if arguments[0] == "search" {
                assert_eq!(document["totalResults"], 0, "{arguments:?}");
                assert_eq!(
                    document["results"].as_array().unwrap().len(),
                    0,
                    "{arguments:?}"
                );
            } else if arguments[0] == "stats" {
                assert_eq!(document["videos"], 0, "{arguments:?}");
                assert_eq!(
                    document["folders"].as_array().unwrap().len(),
                    0,
                    "{arguments:?}"
                );
            } else {
                assert_eq!(
                    document["folders"].as_array().unwrap().len(),
                    0,
                    "{arguments:?}"
                );
            }
        } else if arguments.contains(&"--count") {
            assert_eq!(stdout, "0\n", "{arguments:?}");
            assert!(stderr.is_empty(), "{arguments:?}");
        } else if arguments.contains(&"--verbose") {
            assert!(stdout.is_empty(), "{arguments:?}");
            assert!(stderr.contains("RESULTS RETURNED"), "{stderr:?}");
        } else if arguments[0] == "search" {
            assert!(stdout.is_empty(), "{arguments:?}");
            assert!(stderr.is_empty(), "{arguments:?}");
        } else if arguments[0] == "stats" {
            assert!(stdout.contains("Videos:   0"), "{stdout:?}");
            assert!(stdout.contains("Folders:  0"), "{stdout:?}");
            assert!(stderr.is_empty(), "{arguments:?}");
        } else {
            assert!(stdout.is_empty(), "{arguments:?}");
            assert!(stderr.is_empty(), "{arguments:?}");
        }
    }
    assert!(files_under(home.path()).is_empty());
}

fn files_under(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[test]
fn version_flag_prints_the_ts_version() {
    let output = run_ts(&["-v"], &[]);

    assert_eq!(output.status.code(), Some(0));
    let version = String::from_utf8(output.stdout).unwrap();
    assert!(
        version.starts_with("ts ") && version.contains("(Git SHA "),
        "{version}"
    );
    assert!(output.stderr.is_empty());
}

/// A scratch HOME shared across several `ts` runs, so one run's writes
/// are the next run's reads — exactly how the daemon sees them.
struct ScratchHome {
    home: tempfile::TempDir,
    data_home: PathBuf,
}

fn scratch_home() -> ScratchHome {
    let home = tempdir().unwrap();
    let data_home = home.path().join("data");
    fs::create_dir_all(&data_home).unwrap();
    ScratchHome { home, data_home }
}

fn run_at(home: &ScratchHome, arguments: &[&str]) -> Output {
    run_ts_with_home(arguments, home.home.path(), &home.data_home, None, None)
}

/// Like `run_at`, but with the child's working directory set, so relative
/// shell spellings resolve the way they do for a real invocation.
fn run_at_in(home: &ScratchHome, cwd: &Path, arguments: &[&str]) -> Output {
    run_ts_with_home(
        arguments,
        home.home.path(),
        &home.data_home,
        None,
        Some(cwd),
    )
}

/// The thumbnail cache dir a scratch child resolves: its scratch TMPDIR
/// plus the uid-scoped cache name, mirroring
/// `toka_search::thumbnails::cache_output_root` under the child's
/// environment. The child runs as this user, so its uid is ours.
fn thumbnail_cache_dir(home: &Path) -> PathBuf {
    use toka_search::service::{Host, SystemHost};
    let uid = SystemHost::for_query().map(|host| host.uid()).unwrap_or(0);
    home.join("tmp").join(format!(
        "{}-{uid}",
        toka_search::config::THUMBNAIL_CACHE_DIR_NAME
    ))
}

fn create_image(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    fs::write(&path, b"fixture image").unwrap();
    path
}

#[test]
fn folders_add_then_list_shows_the_new_folder() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    let media_argument = media.to_string_lossy().into_owned();

    let output = run_at(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("Added {}\n", media.display())
    );
    assert!(output.stderr.is_empty());

    // A second process reads what the first wrote: the `roots` row the
    // daemon's reconcile picks up. Nothing scans here — no daemon runs in
    // the test — so the folder stays pending.
    let output = run_at(&home, &["folders", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    let folders = document["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["path"], media.to_string_lossy().into_owned());
    assert_eq!(folders[0]["status"], "pending");
    assert!(output.stderr.is_empty());
}

#[test]
fn folders_add_json_is_a_versioned_document() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media_argument = media.to_string_lossy().into_owned();

    let output = run_at(&home, &["folders", "add", &media_argument, "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["folders"].as_array().unwrap().len(), 1);
    assert!(output.stderr.is_empty());
}

#[test]
fn folders_add_twice_is_an_error() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media_argument = media.to_string_lossy().into_owned();

    let output = run_at(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));

    let output = run_at(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "ts folders add: That folder is already covered by a Toka search folder.\n"
    );
}

#[test]
fn folders_add_of_a_missing_folder_is_an_error() {
    let home = scratch_home();
    let missing = home.home.path().join("no-such-folder");
    let missing_argument = missing.to_string_lossy().into_owned();

    let output = run_at(&home, &["folders", "add", &missing_argument]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("ts folders add: Toka could not open that folder: "),
        "{stderr}"
    );
}

#[test]
fn folders_remove_after_add_empties_the_list() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    let media_argument = media.to_string_lossy().into_owned();

    let output = run_at(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));

    let output = run_at(&home, &["folders", "remove", &media_argument]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("Removed {}\n", media.display())
    );
    assert!(output.stderr.is_empty());

    let output = run_at(&home, &["folders", "list"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn folders_remove_json_is_a_versioned_document() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media_argument = media.to_string_lossy().into_owned();

    let output = run_at(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));

    let output = run_at(&home, &["folders", "remove", &media_argument, "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["folders"].as_array().unwrap().len(), 0);
    assert!(output.stderr.is_empty());
}

#[test]
fn folders_remove_of_an_unknown_folder_is_an_error() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media_argument = media.to_string_lossy().into_owned();

    let output = run_at(&home, &["folders", "remove", &media_argument]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("ts folders remove: {media_argument} is not one of Toka's search folders.\n")
    );
}

#[test]
fn folders_remove_finds_a_folder_deleted_from_disk() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    let media_argument = media.to_string_lossy().into_owned();

    let output = run_at(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));
    fs::remove_dir(&media).unwrap();

    let output = run_at(&home, &["folders", "remove", &media_argument]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("Removed {}\n", media.display())
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn folders_add_with_a_relative_path_stores_the_canonical_spelling() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let canonical = media.canonicalize().unwrap();

    // What was typed is relative; what lands in the index is absolute.
    let output = run_at_in(&home, home.home.path(), &["folders", "add", "media"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("Added {}\n", canonical.display())
    );
    assert!(output.stderr.is_empty());

    let output = run_at(&home, &["folders", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    let folders = document["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["path"], canonical.to_string_lossy().into_owned());
    assert!(output.stderr.is_empty());
}

#[test]
fn folders_remove_with_a_relative_path_finds_a_folder_deleted_from_disk() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let canonical = media.canonicalize().unwrap();

    let output = run_at_in(&home, home.home.path(), &["folders", "add", "media"]);
    assert_eq!(output.status.code(), Some(0));
    fs::remove_dir(&media).unwrap();

    // The same shell spelling that added it still removes it, even
    // though nothing on disk resolves anymore.
    let output = run_at_in(&home, home.home.path(), &["folders", "remove", "media"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("Removed {}\n", canonical.display())
    );
    assert!(output.stderr.is_empty());

    let output = run_at(&home, &["folders", "list"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn folders_add_of_a_file_matches_the_app() {
    // Both go through the same manager, which only takes directories.
    let home = scratch_home();
    let elsewhere = tempdir().unwrap();
    let file = create_video(elsewhere.path(), 0);
    let file_argument = file.to_string_lossy().into_owned();

    let output = run_at(&home, &["folders", "add", &file_argument]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "ts folders add: Choose a folder to search.\n"
    );
    assert!(files_under(home.home.path()).is_empty());
}

#[test]
fn folders_remove_with_no_index_errors_without_creating_state() {
    let home = scratch_home();

    let output = run_at(&home, &["folders", "remove", "/media/movies"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "ts folders remove: /media/movies is not one of Toka's search folders.\n"
    );
    // Unlike the app's remove flow, resolving first means a missing
    // index never gets a database created for it.
    assert!(files_under(home.home.path()).is_empty());
}

/// Where the child resolves the index: dirs honors XDG_DATA_HOME on
/// Linux but derives it from HOME on macOS.
fn scratch_database(home: &ScratchHome) -> PathBuf {
    #[cfg(target_os = "macos")]
    let database = home
        .home
        .path()
        .join("Library/Application Support")
        .join(toka_search::config::APP_DATA_DIR_NAME)
        .join(toka_search::config::INDEX_FILE_NAME);
    #[cfg(not(target_os = "macos"))]
    let database = home
        .data_home
        .join(toka_search::config::APP_DATA_DIR_NAME)
        .join(toka_search::config::INDEX_FILE_NAME);
    database
}

fn stats_videos(home: &ScratchHome) -> serde_json::Value {
    let output = run_at(home, &["stats", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap()
}

#[test]
fn sync_path_indexes_a_file_added_after_the_scan() {
    use toka_search::owned_index::{OwnedIndex, OwnedIndexManager};

    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    let media_argument = media.to_string_lossy().into_owned();
    create_video(&media, 0);

    // The folder goes in through the CLI; the scan stands in for the
    // daemon, which has no test double in this phase.
    let output = run_at(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));
    let database = scratch_database(&home);
    let manager = OwnedIndexManager::under(database.clone());
    let state = manager.folders().unwrap();
    OwnedIndex::open(&database)
        .unwrap()
        .scan_root(&state[0].id, &state[0].path, |_| {})
        .unwrap();
    assert_eq!(stats_videos(&home)["videos"], 1);

    let added = create_video(&media, 1);
    let added_argument = added.to_string_lossy().into_owned();
    let output = run_at(&home, &["sync-path", &added_argument]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    assert_eq!(stats_videos(&home)["videos"], 2);
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn sync_rename_moves_a_file_to_its_new_name() {
    use toka_search::owned_index::{OwnedIndex, OwnedIndexManager};

    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    let media_argument = media.to_string_lossy().into_owned();
    let clip = create_video(&media, 0);

    let output = run_at(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));
    let database = scratch_database(&home);
    let manager = OwnedIndexManager::under(database.clone());
    let state = manager.folders().unwrap();
    OwnedIndex::open(&database)
        .unwrap()
        .scan_root(&state[0].id, &state[0].path, |_| {})
        .unwrap();

    // Tagging renames the file on disk; the app then tells the index the
    // file moved. Without the sync the new name is unsearchable.
    let renamed = media.join("family-trip-00-tagged.mp4");
    fs::rename(&clip, &renamed).unwrap();
    let output = run_at(&home, &["search", "tagged"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());

    let from = clip.to_string_lossy().into_owned();
    let to = renamed.to_string_lossy().into_owned();
    let output = run_at(&home, &["sync-rename", &from, &to]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    let output = run_at(&home, &["search", "tagged"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", renamed.display())
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn sync_commands_with_no_index_are_silent_noops() {
    let home = scratch_home();

    for arguments in [
        vec!["sync-path", "/media/movies/clip.mp4"],
        vec!["sync-rename", "/media/old.mp4", "/media/new.mp4"],
    ] {
        let output = run_at(&home, &arguments);
        assert_eq!(output.status.code(), Some(0), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}");
    }
    // Syncing must never create an index on its own, exactly as in the
    // app: only a folder add brings the database into being.
    assert!(files_under(home.home.path()).is_empty());
}

/// Adds `media` through the CLI and scans it standing in for the daemon,
/// which has no test double in this phase.
fn add_and_scan(home: &ScratchHome, media: &Path) {
    use toka_search::owned_index::{OwnedIndex, OwnedIndexManager};

    let media_argument = media.to_string_lossy().into_owned();
    let output = run_at(home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));
    let database = scratch_database(home);
    let manager = OwnedIndexManager::under(database.clone());
    let folders = manager.folders().unwrap();
    OwnedIndex::open(&database)
        .unwrap()
        .scan_root(&folders[0].id, &folders[0].path, |_| {})
        .unwrap();
}

#[test]
fn sync_path_removes_a_deleted_file_from_the_index() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    create_video(&media, 0);
    add_and_scan(&home, &media);
    assert_eq!(stats_videos(&home)["videos"], 1);

    let clip = media.join("family-trip-00.mp4");
    fs::remove_file(&clip).unwrap();
    let output = run_at(&home, &["sync-path", &clip.to_string_lossy()]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    assert_eq!(stats_videos(&home)["videos"], 0);
}

#[test]
fn sync_path_accepts_a_relative_path() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    add_and_scan(&home, &media);

    create_video(&media, 1);
    let output = run_at_in(&home, &media, &["sync-path", "family-trip-01.mp4"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    assert_eq!(stats_videos(&home)["videos"], 1);
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn sync_rename_accepts_relative_paths() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    create_video(&media, 0);
    add_and_scan(&home, &media);

    fs::rename(
        media.join("family-trip-00.mp4"),
        media.join("family-trip-00-tagged.mp4"),
    )
    .unwrap();
    let output = run_at_in(
        &home,
        &media,
        &[
            "sync-rename",
            "family-trip-00.mp4",
            "family-trip-00-tagged.mp4",
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    let output = run_at(&home, &["search", "tagged"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", media.join("family-trip-00-tagged.mp4").display())
    );
    assert!(output.stderr.is_empty());
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn sync_rename_moves_a_directory_to_its_new_name() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(media.join("sub")).unwrap();
    let media = media.canonicalize().unwrap();
    create_video(&media.join("sub"), 0);
    add_and_scan(&home, &media);

    fs::rename(media.join("sub"), media.join("episodes")).unwrap();
    let output = run_at_in(&home, &media, &["sync-rename", "sub", "episodes"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    let output = run_at(&home, &["search", "family"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "{}\n",
            media.join("episodes").join("family-trip-00.mp4").display()
        )
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn sync_rename_of_a_search_folder_repoints_its_roots_row() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    create_video(&media, 0);
    add_and_scan(&home, &media);
    assert_eq!(stats_videos(&home)["videos"], 1);

    let moved = home.home.path().join("media-moved");
    fs::rename(&media, &moved).unwrap();
    let moved = moved.canonicalize().unwrap();
    let output = run_at(
        &home,
        &[
            "sync-rename",
            &media.to_string_lossy(),
            &moved.to_string_lossy(),
        ],
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());

    // The folder kept its identity but moved: the roots row names the
    // new path and the files came along.
    let output = run_at(&home, &["folders", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    let folders = document["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["path"], moved.to_string_lossy().into_owned());
    assert_eq!(stats_videos(&home)["videos"], 1);
}

#[test]
fn sync_rename_across_watched_folders_is_an_error() {
    let home = scratch_home();
    let movies = home.home.path().join("movies");
    let series = home.home.path().join("series");
    fs::create_dir_all(&movies).unwrap();
    fs::create_dir_all(&series).unwrap();
    let movies = movies.canonicalize().unwrap();
    let series = series.canonicalize().unwrap();
    for folder in [&movies, &series] {
        let output = run_at(&home, &["folders", "add", &folder.to_string_lossy()]);
        assert_eq!(output.status.code(), Some(0));
    }

    // A move across folders is not a rename the index can apply: both
    // sides must sit under one watched folder.
    let from = movies.join("clip.mp4");
    let to = series.join("clip.mp4");
    fs::write(&from, b"video").unwrap();
    fs::rename(&from, &to).unwrap();
    let output = run_at(
        &home,
        &[
            "sync-rename",
            &from.to_string_lossy(),
            &to.to_string_lossy(),
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!(
            "ts sync-rename: {} and {} are not under the same watched folder.\n",
            from.display(),
            to.display()
        )
    );

    // The failed rename touched nothing: both folders still listed.
    let output = run_at(&home, &["folders", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["folders"].as_array().unwrap().len(), 2);
}

#[test]
fn sync_path_outside_any_watched_folder_is_an_error() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    add_and_scan(&home, &media);

    // Watched or not is about the folders, not about existence: both an
    // existing outsider and a missing path fail the same way.
    let outsider = home.home.path().join("outside.mp4");
    fs::write(&outsider, b"video").unwrap();
    let missing = home.home.path().join("missing.mp4");
    for path in [&outsider, &missing] {
        let argument = path.to_string_lossy().into_owned();
        let output = run_at(&home, &["sync-path", &argument]);
        assert_eq!(output.status.code(), Some(1), "{argument}");
        assert!(output.stdout.is_empty(), "{argument}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!("ts sync-path: {argument} is not in any watched folder.\n"),
            "{argument}"
        );
    }
}

#[test]
fn sync_errors_carry_the_command_prefix() {
    // The Phase 6 error contract: a failure names the command that failed.
    let home = scratch_home();
    let database = scratch_database(&home);
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    fs::write(&database, b"not a database").unwrap();

    for (arguments, prefix) in [
        (vec!["sync-path", "clip.mp4"], "ts sync-path: "),
        (
            vec!["sync-rename", "old.mp4", "new.mp4"],
            "ts sync-rename: ",
        ),
    ] {
        let output = run_at(&home, &arguments);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.starts_with(prefix), "{stderr:?}");
    }
}

#[cfg(unix)]
#[test]
fn sync_path_accepts_symlinked_and_unclean_spellings() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(media.join("sub")).unwrap();
    let media = media.canonicalize().unwrap();
    add_and_scan(&home, &media);
    let link = home.home.path().join("link");
    std::os::unix::fs::symlink(&media, &link).unwrap();

    // Through a symlinked ancestor with a `..` segment: the same file
    // the canonical spelling names, indexed once.
    let clip = media.join("clip.mp4");
    fs::write(&clip, b"video").unwrap();
    let unclean = link.join("sub").join("..").join("clip.mp4");
    let output = run_at(&home, &["sync-path", &unclean.to_string_lossy()]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(stats_videos(&home)["videos"], 1);

    // And the canonical spelling afterwards refreshes the same row.
    let output = run_at(&home, &["sync-path", &clip.to_string_lossy()]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(stats_videos(&home)["videos"], 1);
}

#[test]
fn thumbnail_of_an_image_prints_itself() {
    let home = scratch_home();
    let image = create_image(home.home.path(), "photo.jpg");
    let image_argument = image.to_string_lossy().into_owned();
    // The fast path reports the canonical spelling, like the
    // generator-backed paths, not the spelling as typed.
    let canonical = image.canonicalize().unwrap().to_string_lossy().into_owned();

    let output = run_at(&home, &["thumbnail", &image_argument]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{canonical}\n")
    );
    assert!(output.stderr.is_empty());

    let output = run_at(&home, &["thumbnail", &image_argument, "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["thumbnail"], canonical);
    assert!(output.stderr.is_empty());
}

#[test]
fn thumbnail_of_an_undecodable_video_is_an_error() {
    let home = scratch_home();
    let clip = create_video(home.home.path(), 0);
    // Not a video at all: ffmpeg fails, or is not even there. Either
    // way no thumbnail comes back.
    let clip_argument = clip.to_string_lossy().into_owned();

    let output = run_at(&home, &["thumbnail", &clip_argument]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("ts thumbnail: Toka could not make a thumbnail for {clip_argument}.\n")
    );
}

#[test]
fn preview_of_an_image_prints_itself() {
    let home = scratch_home();
    let image = create_image(home.home.path(), "photo.jpg");
    let image_argument = image.to_string_lossy().into_owned();
    // Canonical spelling, as with thumbnails.
    let canonical = image.canonicalize().unwrap().to_string_lossy().into_owned();

    let output = run_at(&home, &["preview", &image_argument]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{canonical}\n")
    );
    assert!(output.stderr.is_empty());

    let output = run_at(&home, &["preview", &image_argument, "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["previews"], serde_json::json!([canonical]));
    assert!(output.stderr.is_empty());
}

#[test]
fn preview_of_an_undecodable_video_is_an_error() {
    let home = scratch_home();
    let clip = create_video(home.home.path(), 0);
    let clip_argument = clip.to_string_lossy().into_owned();

    let output = run_at(&home, &["preview", &clip_argument]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("ts preview: Toka could not make a preview for {clip_argument}.\n")
    );
}

#[test]
fn thumbnail_of_a_missing_image_is_an_honest_error() {
    // The image fast path needs the file on disk; a missing image falls
    // through to generation and fails instead of echoing a path to
    // nothing. Pinned as is — Phase 6 owns the app-parity decision.
    let home = scratch_home();
    let missing = home.home.path().join("gone.jpg");
    let missing_argument = missing.to_string_lossy().into_owned();

    let output = run_at(&home, &["thumbnail", &missing_argument]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("ts thumbnail: Toka could not make a thumbnail for {missing_argument}.\n")
    );
}

#[test]
fn preview_of_a_missing_image_is_an_honest_error() {
    // Same fall-through as thumbnails, pinned the same way.
    let home = scratch_home();
    let missing = home.home.path().join("gone.jpg");
    let missing_argument = missing.to_string_lossy().into_owned();

    let output = run_at(&home, &["preview", &missing_argument]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        format!("ts preview: Toka could not make a preview for {missing_argument}.\n")
    );
}

/// Two same-named files requested by relative path from different
/// directories must not share one thumbnail cache entry: the keys also
/// cover size and modification time, so both are pinned equal here and
/// only the directory differs. The failure markers stand in for the
/// cache entries — undecodable input writes one marker per key whether
/// or not ffmpeg is installed.
fn same_named_files_in_different_directories_keep_distinct_cache_entries(command: &str) {
    let home = scratch_home();
    for name in ["a", "b"] {
        let directory = home.home.path().join(name);
        fs::create_dir(&directory).unwrap();
        let clip = directory.join("clip.mp4");
        fs::write(&clip, b"fixture video").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&clip)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .unwrap();
    }

    for name in ["a", "b"] {
        let output = run_at_in(&home, &home.home.path().join(name), &[command, "clip.mp4"]);
        assert_eq!(output.status.code(), Some(1), "{command} from {name}");
        assert!(output.stdout.is_empty());
    }

    let cache = thumbnail_cache_dir(home.home.path());
    let markers = fs::read_dir(&cache)
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "failed")
        })
        .collect::<Vec<_>>();
    assert_eq!(markers.len(), 2, "{command}: {markers:?}");
}

#[test]
fn thumbnails_of_same_named_files_in_different_directories_do_not_collide() {
    same_named_files_in_different_directories_keep_distinct_cache_entries("thumbnail");
}

#[test]
fn previews_of_same_named_files_in_different_directories_do_not_collide() {
    same_named_files_in_different_directories_keep_distinct_cache_entries("preview");
}

// No success-path test for real videos: generating from a valid file
// needs ffmpeg plus a committed fixture and stays environment-dependent.
// The app's hover path already covers generation; here the undecodable
// and missing inputs pin the honest failures.

#[test]
fn folders_add_with_an_empty_path_is_a_usage_error() {
    // The classic empty-variable footgun: rejected before anything can
    // resolve it against the working directory, and nothing is stored.
    let home = scratch_home();

    let output = run_at(&home, &["folders", "add", ""]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("A path must not be empty."), "{stderr}");
    assert!(stderr.contains("Usage: ts folders add"), "{stderr}");

    let output = run_at(&home, &["folders", "list"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
}

#[test]
fn folders_remove_with_an_empty_path_is_a_usage_error() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media_argument = media.to_string_lossy().into_owned();
    let output = run_at(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));

    let output = run_at(&home, &["folders", "remove", ""]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("A path must not be empty."), "{stderr}");
    assert!(stderr.contains("Usage: ts folders remove"), "{stderr}");

    // The watched folder survives the rejected remove.
    let output = run_at(&home, &["folders", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["folders"].as_array().unwrap().len(), 1);
}

#[test]
fn sync_path_with_an_empty_path_is_a_usage_error() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    add_and_scan(&home, &media);

    let output = run_at(&home, &["sync-path", ""]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("A path must not be empty."), "{stderr}");
    assert!(stderr.contains("Usage: ts sync-path"), "{stderr}");

    // The rejected sync indexed nothing.
    assert_eq!(stats_videos(&home)["videos"], 0);
}

#[test]
fn thumbnail_with_an_empty_path_is_a_usage_error() {
    let home = scratch_home();

    let output = run_at(&home, &["thumbnail", ""]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("A path must not be empty."), "{stderr}");
    assert!(stderr.contains("Usage: ts thumbnail"), "{stderr}");

    // The generator never ran, so no cache entry exists.
    let cache = thumbnail_cache_dir(home.home.path());
    let entries = fs::read_dir(&cache)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(entries, 0);
}

#[test]
fn thumbnail_of_a_relative_image_reports_the_canonical_spelling() {
    let home = scratch_home();
    let image = create_image(home.home.path(), "photo.jpg");
    let canonical = image.canonicalize().unwrap().to_string_lossy().into_owned();
    assert!(Path::new(&canonical).is_absolute());

    // Typed relative; reported absolute, like the video paths.
    let output = run_at_in(&home, home.home.path(), &["thumbnail", "photo.jpg"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{canonical}\n")
    );
    assert!(output.stderr.is_empty());

    let output = run_at_in(
        &home,
        home.home.path(),
        &["thumbnail", "photo.jpg", "--json"],
    );
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["thumbnail"], canonical);
    assert!(output.stderr.is_empty());
}

#[cfg(unix)]
#[test]
fn folders_add_through_a_symlink_echoes_the_stored_spelling() {
    let home = scratch_home();
    let real = home.home.path().join("real");
    fs::create_dir_all(&real).unwrap();
    let link = home.home.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let link_argument = link.to_string_lossy().into_owned();
    let canonical = real.canonicalize().unwrap();

    // Typed through the link; the echo names what the manager stored.
    let output = run_at(&home, &["folders", "add", &link_argument]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("Added {}\n", canonical.display())
    );
    assert!(output.stderr.is_empty());

    let output = run_at(&home, &["folders", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    let folders = document["folders"].as_array().unwrap();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0]["path"], canonical.to_string_lossy().into_owned());
}

#[test]
fn status_with_no_index_reports_a_stopped_daemon_with_exit_3() {
    let home = scratch_home();

    let output = run_at(&home, &["status"]);

    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Daemon:   not running\n"), "{stdout}");
    assert!(output.stderr.is_empty());
}

#[test]
fn status_json_with_no_index_is_a_stopped_document() {
    let home = scratch_home();

    let output = run_at(&home, &["status", "--json"]);

    assert_eq!(output.status.code(), Some(3));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["running"], false);
    assert!(document["activity"].is_null());
    assert_eq!(document["queue"]["pendingRoots"], 0);
    assert!(output.stderr.is_empty());
}

#[test]
fn status_creates_no_state_without_an_index() {
    let home = scratch_home();

    for arguments in [
        &["status"][..],
        &["status", "--json"][..],
        &["status", "--watch"][..],
    ] {
        let output = run_ts_with_home(arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(3), "{arguments:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}");
    }
    assert!(files_under(home.home.path()).is_empty());
}

#[test]
fn status_on_a_newer_schema_errors_instead_of_reporting() {
    let home = scratch_home();
    write_newer_schema_database(&index_database_path(home.home.path(), &home.data_home));

    for arguments in [vec!["status"], vec!["status", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("schema version 999"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn status_on_a_corrupt_index_errors_instead_of_reporting() {
    let home = scratch_home();
    write_corrupt_database(&index_database_path(home.home.path(), &home.data_home));

    for arguments in [vec!["status"], vec!["status", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("could not use its search index"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn status_with_an_unreadable_index_errors_instead_of_reporting_stopped() {
    let (_base, home, data_home, tmpdir) = overlong_env();

    for arguments in [vec!["status"], vec!["status", "--json"]] {
        let output = run_ts_with_raw_envs(&arguments, &home, &data_home, &tmpdir);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("could not read its search index"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn status_with_a_directory_at_the_index_path_errors_instead_of_reporting_stopped() {
    let home = scratch_home();
    let database = index_database_path(home.home.path(), &home.data_home);
    fs::create_dir_all(&database).unwrap();

    for arguments in [vec!["status"], vec!["status", "--json"]] {
        let output = run_ts_with_home(&arguments, home.home.path(), &home.data_home, None, None);
        assert_eq!(output.status.code(), Some(1), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("is not a file"), "{arguments:?}: {stderr}");
    }
}

#[test]
fn status_with_an_unknown_option_prints_usage() {
    let home = scratch_home();

    let output = run_at(&home, &["status", "--verbose"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Usage: ts status [--json] [--watch]\n"
    );
}

#[test]
fn daemon_help_prints_without_starting_anything() {
    for arguments in [&["daemon", "--help"][..], &["daemon", "run", "--help"][..]] {
        let output = run_ts(arguments, &[]);
        assert_eq!(output.status.code(), Some(0), "{arguments:?}");
        assert!(!output.stdout.is_empty(), "{arguments:?}");
        assert!(output.stderr.is_empty(), "{arguments:?}");
    }
}

#[test]
fn daemon_without_a_subcommand_prints_usage() {
    let output = run_ts(&["daemon"], &[]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("Usage: ts daemon"));
}

#[test]
fn daemon_run_indexes_added_folders_until_stopped() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    create_video(&media, 0);
    create_video(&media, 1);

    // Managed inline (rather than `with_daemon`) so a timeout can report
    // whether the daemon is still alive and what it logged before the
    // kill discards the evidence.
    let mut daemon = spawn_daemon(&home);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The daemon publishes its first heartbeat within seconds.
        assert!(
            wait_for_status(&home, Duration::from_secs(30), |document| {
                document["running"] == true
            }),
            "the daemon never published a heartbeat"
        );

        let media_argument = media.to_string_lossy().into_owned();
        let output = run_at(&home, &["folders", "add", &media_argument]);
        assert_eq!(output.status.code(), Some(0));

        // The daemon picks the folder up and drains its queue to idle.
        // The wait pairs status with stats: an idle report alone could
        // predate the add, but idle plus two indexed videos proves the
        // daemon saw the folder and scanned it.
        let idle_and_empty = |document: &serde_json::Value| {
            document["running"] == true
                && document["activity"] == "idle"
                && document["queue"]["pendingRoots"] == 0
                && document["queue"]["filesRemaining"] == 0
                && document["queue"]["pendingRepairs"] == 0
                && document["queue"]["pendingThumbnails"] == 0
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        let (finished, last_status, last_videos) = loop {
            let output = run_at(&home, &["status", "--json"]);
            let status_text = String::from_utf8_lossy(&output.stdout).into_owned();
            let idle = serde_json::from_str::<serde_json::Value>(&status_text)
                .map(|document| idle_and_empty(&document))
                .unwrap_or(false);
            let videos_text = if idle {
                let output = run_at(&home, &["stats", "--json"]);
                Some(String::from_utf8_lossy(&output.stdout).into_owned())
            } else {
                None
            };
            let videos = videos_text
                .as_ref()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                .and_then(|document| document["videos"].as_u64());
            if videos == Some(2) {
                // Both halves agree — but the idle heartbeat may predate
                // the stats read (a stale pre-add beat plus committed
                // stats would accept while the daemon is still busy).
                // Heartbeats land every two seconds, so settling past two
                // intervals means the next heartbeat must postdate the
                // stats read; only a fresh idle report then proves the
                // daemon drained its queue after indexing.
                thread::sleep(Duration::from_secs(5));
                let output = run_at(&home, &["status", "--json"]);
                let verify_text = String::from_utf8_lossy(&output.stdout).into_owned();
                let fresh_idle = serde_json::from_str::<serde_json::Value>(&verify_text)
                    .map(|document| {
                        idle_and_empty(&document)
                            && document["heartbeatAgeSecs"]
                                .as_u64()
                                .is_some_and(|age| age <= 2)
                    })
                    .unwrap_or(false);
                if fresh_idle {
                    break (true, verify_text, videos_text);
                }
            }
            if Instant::now() >= deadline {
                break (false, status_text, videos_text);
            }
            thread::sleep(Duration::from_millis(200));
        };
        assert!(
            finished,
            "the daemon never finished indexing; last status: {last_status} last stats: {}",
            last_videos.as_deref().unwrap_or("<stats never read>"),
        );

        // The heartbeat stays fresh while the daemon runs.
        let output = run_at(&home, &["status", "--json"]);
        assert_eq!(output.status.code(), Some(0));
        let document: serde_json::Value =
            serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
        assert!(document["heartbeatAgeSecs"].as_u64().unwrap() < 10);
        assert!(document["uptimeSecs"].as_u64().is_some());

        // The accepted pair proved quiescence; the final report must use
        // an observation consistent with it, not a single racy read.
        let activity_deadline = Instant::now() + Duration::from_secs(30);
        let stdout = loop {
            let output = run_at(&home, &["status"]);
            assert_eq!(output.status.code(), Some(0));
            let stdout = String::from_utf8(output.stdout).unwrap();
            if stdout.contains("Activity: idle") {
                break stdout;
            }
            assert!(
                Instant::now() < activity_deadline,
                "the daemon never settled back to idle; last status: {stdout}"
            );
            thread::sleep(Duration::from_millis(200));
        };
        assert!(stdout.contains("Daemon:   running"), "{stdout}");
    }));
    // Whether the body passed or failed: is the daemon still alive, what
    // did it say on stderr, and what did it log?
    let daemon_state = match daemon.try_wait() {
        Ok(None) => "alive".to_owned(),
        status => format!("exited: {status:?}"),
    };
    let _ = daemon.kill();
    let daemon_output = daemon.wait_with_output();
    let daemon_stderr = daemon_output
        .as_ref()
        .map(|output| String::from_utf8_lossy(&output.stderr).into_owned())
        .unwrap_or_else(|error| format!("<could not reap the daemon: {error}>"));
    let indexer_log = indexer_log_path(&home);
    let indexer_log_tail = fs::read_to_string(&indexer_log)
        .map(|log| {
            let mut lines: Vec<&str> = log.lines().collect();
            if lines.len() > 20 {
                lines = lines[lines.len() - 20..].to_vec();
            }
            lines.join("\n")
        })
        .unwrap_or_else(|_| "<no indexer log>".to_owned());
    if let Err(payload) = result {
        eprintln!("daemon state at failure: {daemon_state}");
        eprintln!("daemon stderr at failure: {daemon_stderr}");
        eprintln!("indexer log at failure:\n{indexer_log_tail}");
        std::panic::resume_unwind(payload);
    }

    // A dead daemon reads as stopped: the pid is gone even though its
    // last heartbeat is seconds old.
    let output = run_at(&home, &["status"]);
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Daemon:   not running"), "{stdout}");
}

/// Where a scratch home's daemon writes its log: dirs honors
/// XDG_DATA_HOME on Linux but derives it from HOME on macOS.
fn indexer_log_path(home: &ScratchHome) -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = home.home.path().join("Library/Application Support");
    #[cfg(not(target_os = "macos"))]
    let base = home.data_home.clone();
    base.join(toka_search::config::APP_LOG_DIR_NAME)
        .join("logs/indexer.log")
}

/// Rewinds a version 2 index to version 1: no liveness table, no
/// indexable counter, version markers back to 1.
#[cfg(not(feature = "fixtures"))]
fn rewind_to_v1(database: &Path) {
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute_batch(
            "DROP TABLE daemon_state;
             ALTER TABLE scan_runs DROP COLUMN files_indexable;
             UPDATE schema_meta SET value = 1 WHERE key = 'schema_version';",
        )
        .unwrap();
    connection.pragma_update(None, "user_version", 1).unwrap();
}

#[cfg(not(feature = "fixtures"))]
fn schema_user_version(database: &Path) -> i64 {
    rusqlite::Connection::open(database)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

#[cfg(not(feature = "fixtures"))]
fn schema_meta_version(database: &Path) -> i64 {
    rusqlite::Connection::open(database)
        .unwrap()
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

/// A logical dump of the index: every table's row count, in name order.
/// Compared alongside the raw bytes, so a read that rewrote content
/// without changing the file size still fails.
#[cfg(not(feature = "fixtures"))]
fn index_dump(database: &Path) -> Vec<(String, i64)> {
    let connection = rusqlite::Connection::open(database).unwrap();
    let tables: Vec<String> = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    tables
        .into_iter()
        .map(|table| {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |row| {
                    row.get(0)
                })
                .unwrap();
            (table, count)
        })
        .collect()
}

#[cfg(not(feature = "fixtures"))]
fn assert_v1_untouched(database: &Path, bytes: &[u8], dump: &[(String, i64)]) {
    assert_eq!(schema_user_version(database), 1);
    assert_eq!(schema_meta_version(database), 1);
    assert_eq!(index_dump(database), dump);
    assert_eq!(fs::read(database).unwrap(), bytes);
}

#[cfg(not(feature = "fixtures"))]
#[test]
fn read_commands_leave_a_version_1_database_untouched() {
    use toka_search::owned_index::{OwnedIndex, OwnedIndexManager};

    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    let media = media.canonicalize().unwrap();
    create_video(&media, 0);
    create_video(&media, 1);

    // A scanned version 2 index, rewound to version 1.
    let media_argument = media.to_string_lossy().into_owned();
    let output = run_at(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));
    let database = scratch_database(&home);
    let manager = OwnedIndexManager::under(database.clone());
    let folders = manager.folders().unwrap();
    OwnedIndex::open(&database)
        .unwrap()
        .scan_root(&folders[0].id, &folders[0].path, |_| {})
        .unwrap();
    rewind_to_v1(&database);
    let before = fs::read(&database).unwrap();
    let dump = index_dump(&database);

    // Every read command proves it can read version 1, then proves it
    // changed nothing: version markers, logical dump, and raw bytes.
    let output = run_at(&home, &["status"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("Daemon:   not running\n"));
    assert_v1_untouched(&database, &before, &dump);

    let output = run_at(&home, &["status", "--json"]);
    assert_eq!(output.status.code(), Some(3));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["running"], false);
    assert_v1_untouched(&database, &before, &dump);

    // A stopped daemon exits the watch immediately, without waiting.
    let output = run_at(&home, &["status", "--watch"]);
    assert_eq!(output.status.code(), Some(3));
    assert_v1_untouched(&database, &before, &dump);

    let output = run_at(&home, &["stats"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Videos:   2"), "{stdout}");
    assert!(stdout.contains("Folders:  1"), "{stdout}");
    assert_v1_untouched(&database, &before, &dump);

    let output = run_at(&home, &["stats", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["videos"], 2);
    assert_eq!(document["folders"].as_array().unwrap().len(), 1);
    assert_v1_untouched(&database, &before, &dump);

    let output = run_at(&home, &["folders", "list"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{} [ready]\n", media.display())
    );
    assert_v1_untouched(&database, &before, &dump);

    let output = run_at(&home, &["folders", "list", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["folders"].as_array().unwrap().len(), 1);
    assert_v1_untouched(&database, &before, &dump);

    let output = run_at(&home, &["search", "family"]);
    assert_eq!(output.status.code(), Some(0));
    let mut expected = [
        media.join("family-trip-00.mp4").display().to_string(),
        media.join("family-trip-01.mp4").display().to_string(),
    ];
    expected.sort();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        expected.join("\n") + "\n"
    );
    assert_v1_untouched(&database, &before, &dump);

    let output = run_at(&home, &["search", "family", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["totalResults"], 2);
    assert_v1_untouched(&database, &before, &dump);

    // A write migrates: adding a folder moves the database to version 2,
    // and the version 1 content survives the move.
    let more = home.home.path().join("more");
    fs::create_dir_all(&more).unwrap();
    let more_argument = more.to_string_lossy().into_owned();
    let output = run_at(&home, &["folders", "add", &more_argument]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(schema_user_version(&database), 2);
    let output = run_at(&home, &["stats", "--json"]);
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["videos"], 2);
    assert_eq!(document["folders"].as_array().unwrap().len(), 2);

    // So does starting the daemon: rewind, let the heartbeat migrate,
    // and the folders are still all there.
    rewind_to_v1(&database);
    assert_eq!(schema_user_version(&database), 1);
    with_daemon(&home, || {
        assert!(
            wait_for_status(&home, Duration::from_secs(30), |document| {
                document["running"] == true
            }),
            "the daemon never published a heartbeat"
        );
    });
    assert_eq!(schema_user_version(&database), 2);
    let output = run_at(&home, &["folders", "list", "--json"]);
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["folders"].as_array().unwrap().len(), 2);
}

#[test]
fn status_watch_against_a_live_daemon_exits_when_the_queue_drains() {
    let home = scratch_home();
    let media = home.home.path().join("media");
    fs::create_dir_all(&media).unwrap();
    // Enough files that the scan cannot finish before the watch's first
    // read, so the watch provably holds instead of exiting at once.
    for number in 0..200 {
        create_video(&media, number);
    }

    let mut daemon = spawn_daemon(&home);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(
            wait_for_status(&home, Duration::from_secs(30), |document| {
                document["running"] == true
            }),
            "the daemon never published a heartbeat"
        );

        let media_argument = media.to_string_lossy().into_owned();
        let output = run_at(&home, &["folders", "add", &media_argument]);
        assert_eq!(output.status.code(), Some(0));

        // Wait until the daemon publishes outstanding work: the watch
        // must start while the queue is non-empty, or a stale idle
        // heartbeat would end it before the scan even starts. The first
        // non-empty publish lands seconds before the drain, so the
        // watch's first read still sees work outstanding.
        let queue_empty = |document: &serde_json::Value| {
            document["queue"]["pendingRoots"] == 0
                && document["queue"]["filesRemaining"] == 0
                && document["queue"]["pendingRepairs"] == 0
                && document["queue"]["pendingThumbnails"] == 0
        };
        assert!(
            wait_for_status(&home, Duration::from_secs(30), |document| {
                document["running"] == true
                    && (document["activity"] != "idle" || !queue_empty(document))
            }),
            "the daemon never published outstanding work"
        );

        // The watch holds while the daemon scans, then exits 0 on the
        // drained queue. Piped, so no screen clears may appear. The bound
        // is generous on purpose: 200 fixture videos each run real
        // ffprobe/ffmpeg rejection, which is seconds even unloaded and
        // far slower on loaded CI — the bound only guards a real hang.
        let child = ts_command(
            &["status", "--watch"],
            home.home.path(),
            &home.data_home,
            None,
            None,
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
        let output = wait_for_exit_timeout(child, Duration::from_secs(120));
        assert_eq!(output.status.code(), Some(0));
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(!stdout.contains('\x1b'), "{stdout:?}");
        assert!(stdout.contains("Activity: idle"), "{stdout}");
        assert!(
            stdout.matches("Daemon:").count() >= 2,
            "expected the watch to hold for several renders, got:\n{stdout}"
        );
        assert!(output.stderr.is_empty());
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn a_second_daemon_on_the_same_index_exits_with_an_error() {
    let home = scratch_home();

    with_daemon(&home, || {
        assert!(
            wait_for_status(&home, Duration::from_secs(30), |document| {
                document["running"] == true
            }),
            "the first daemon never started"
        );

        let output = run_at(&home, &["daemon", "run"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "ts daemon run: Another Toka indexer daemon is already running.\n"
        );
    });
}

/// A scratch home whose service managers are shims, in the style of the
/// isolated-run `systemctl` shim: `systemctl`, `launchctl`, and `loginctl`
/// resolve to scripts that log every call and answer from state files
/// under the scratch home, so service tests never enable, start, or even
/// query the real user services.
#[cfg(unix)]
struct ShimHome {
    scratch: ScratchHome,
    shim_dir: PathBuf,
    log: PathBuf,
    state: PathBuf,
}

#[cfg(unix)]
const SYSTEMCTL_SHIM: &str = r#"#!/bin/sh
# Installed by the ts service tests: a ts under test must never control
# the real user services. Logs every call and answers from state files.
echo "systemctl $*" >> "$TS_SHIM_LOG"
if [ -n "$TS_SHIM_FAIL" ]; then
  case "$*" in *"$TS_SHIM_FAIL"*) echo "injected failure" >&2; exit 1;; esac
fi
verb="$2"
unit=""
for arg in "$@"; do unit="$arg"; done
unitdir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
active="$TS_SHIM_STATE/active_$unit"
enabled="$TS_SHIM_STATE/enabled_$unit"
case "$verb" in
  is-active)
    if [ -f "$TS_SHIM_STATE/word_$unit" ]; then cat "$TS_SHIM_STATE/word_$unit"; exit 3;
    elif [ "$(cat "$active" 2>/dev/null)" = "1" ]; then echo "active"; exit 0;
    elif [ "$(cat "$TS_SHIM_STATE/activating_$unit" 2>/dev/null)" = "1" ]; then echo "activating"; exit 3;
    elif [ ! -f "$unitdir/$unit" ] && [ ! -f "$HOME/.config/systemd/user/$unit" ]; then echo "inactive"; exit 4;
    else echo "inactive"; exit 3; fi;;
  is-enabled)
    if [ "$(cat "$enabled" 2>/dev/null)" = "1" ]; then echo "enabled"; exit 0;
    else echo "disabled"; exit 1; fi;;
  show)
    line=$(grep '^ExecStart=' "$unitdir/$unit" 2>/dev/null | head -n 1)
    exe=$(printf '%s' "$line" | sed -n 's/^ExecStart="\(.*\)" daemon run$/\1/p')
    # Real systemctl reports the decoded path, as the unit-test fake's
    # show_exec_start does: unescape first, then fold %% to %.
    exe=$(printf '%s' "$exe" | sed -e 's/\\"/"/g' -e 's/\\\\/\\/g' -e 's/%%/%/g')
    if [ -n "$exe" ]; then echo "ExecStart={ path=$exe ; argv[]=$exe daemon run ; ignore_errors=no }";
    else echo "ExecStart="; fi;;
  start) echo 1 > "$active"; rm -f "$TS_SHIM_STATE/activating_$unit";;
  stop) echo 0 > "$active"; rm -f "$TS_SHIM_STATE/activating_$unit";;
  enable) echo 1 > "$enabled";;
  disable) echo 0 > "$enabled";;
  daemon-reload) ;;
esac
exit 0
"#;

#[cfg(unix)]
const LAUNCHCTL_SHIM: &str = r#"#!/bin/sh
# Installed by the ts service tests: a ts under test must never control
# the real launchd agents. Logs every call and answers from state files.
echo "launchctl $*" >> "$TS_SHIM_LOG"
if [ -n "$TS_SHIM_FAIL" ]; then
  case "$*" in *"$TS_SHIM_FAIL"*) echo "injected failure" >&2; exit 1;; esac
fi
case "$1" in
  print)
    if [ "$(cat "$TS_SHIM_STATE/bootstrapped" 2>/dev/null)" = "1" ]; then
      if [ "$(cat "$TS_SHIM_STATE/job_running" 2>/dev/null)" = "1" ]; then job="running";
      else job="exited"; fi
      printf '%s = {\n\tstate = %s;\n' "$2" "$job"
      if [ -f "$TS_SHIM_STATE/program" ]; then printf '\tprogram = %s;\n' "$(cat "$TS_SHIM_STATE/program")"; fi
      printf '}\n'
      exit 0
    else
      echo "Could not find service $2 in domain" >&2; exit 1
    fi;;
  bootstrap)
    echo 1 > "$TS_SHIM_STATE/bootstrapped"; echo 1 > "$TS_SHIM_STATE/job_running"
    exe=$(grep -A3 '<key>ProgramArguments</key>' "$3" 2>/dev/null | grep '<string>' | head -n 1 | sed 's/.*<string>//;s/<\/string>.*//')
    # Real launchd reports the decoded path: entities unescaped (&lt;
    # and &gt; first, &amp; last), as xml_unescape does.
    exe=$(printf '%s' "$exe" | sed -e 's/&lt;/</g' -e 's/&gt;/>/g' -e 's/&amp;/\&/g')
    if [ -n "$exe" ]; then printf '%s' "$exe" > "$TS_SHIM_STATE/program"; else rm -f "$TS_SHIM_STATE/program"; fi;;
  bootout) echo 0 > "$TS_SHIM_STATE/bootstrapped"; echo 0 > "$TS_SHIM_STATE/job_running";;
  kickstart)
    if [ "$(cat "$TS_SHIM_STATE/bootstrapped" 2>/dev/null)" = "1" ]; then
      echo 1 > "$TS_SHIM_STATE/job_running"; exit 0
    else echo "Could not find service" >&2; exit 1; fi;;
esac
exit 0
"#;

#[cfg(unix)]
const LOGINCTL_SHIM: &str = r#"#!/bin/sh
# Installed by the ts service tests: lingering answers come from a state
# file, never from the real login manager.
echo "loginctl $*" >> "$TS_SHIM_LOG"
if [ -n "$TS_SHIM_FAIL" ]; then
  case "$*" in *"$TS_SHIM_FAIL"*) echo "injected failure" >&2; exit 1;; esac
fi
case "$1" in
  show-user) [ "$(cat "$TS_SHIM_STATE/lingering" 2>/dev/null)" = "1" ] && echo yes || echo no;;
  enable-linger) echo 1 > "$TS_SHIM_STATE/lingering";;
esac
exit 0
"#;

#[cfg(unix)]
fn shim_home() -> ShimHome {
    let scratch = scratch_home();
    let shim_dir = scratch.home.path().join("shim");
    let state = scratch.home.path().join("shim-state");
    fs::create_dir_all(&shim_dir).unwrap();
    fs::create_dir_all(&state).unwrap();
    for (name, script) in [
        ("systemctl", SYSTEMCTL_SHIM),
        ("launchctl", LAUNCHCTL_SHIM),
        ("loginctl", LOGINCTL_SHIM),
    ] {
        let path = shim_dir.join(name);
        fs::write(&path, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let log = scratch.home.path().join("shim.log");
    ShimHome {
        scratch,
        shim_dir,
        log,
        state,
    }
}

#[cfg(unix)]
impl ShimHome {
    fn home(&self) -> &Path {
        self.scratch.home.path()
    }

    fn calls(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[cfg(unix)]
fn run_shimmed(home: &ShimHome, arguments: &[&str]) -> Output {
    run_shimmed_with_fail(home, arguments, None, &[])
}

/// Like [`run_shimmed`], but every shimmed invocation whose arguments
/// contain `fail` exits 1, so failure paths are testable. Extra
/// environment entries override the harness defaults per run.
#[cfg(unix)]
fn run_shimmed_with_fail(
    home: &ShimHome,
    arguments: &[&str],
    fail: Option<&str>,
    extra_env: &[(&str, &str)],
) -> Output {
    spawn_and_wait(shimmed_command(
        home,
        Path::new(env!("CARGO_BIN_EXE_ts")),
        arguments,
        fail,
        extra_env,
    ))
}

/// The shimmed [`Command`] behind [`run_shimmed_with_fail`], split out so
/// tests can stage another binary in place of the test target, or spawn
/// overlapping installs from threads.
#[cfg(unix)]
fn shimmed_command(
    home: &ShimHome,
    binary: &Path,
    arguments: &[&str],
    fail: Option<&str>,
    extra_env: &[(&str, &str)],
) -> Command {
    let mut command = ts_command_for_binary(
        binary,
        arguments,
        home.scratch.home.path(),
        &home.scratch.data_home,
        None,
        None,
    );
    let path = std::env::var("PATH").unwrap_or_default();
    command.env("PATH", format!("{}:{path}", home.shim_dir.display()));
    command.env("TS_SHIM_LOG", &home.log);
    command.env("TS_SHIM_STATE", &home.state);
    // A fixture login name: lingering flows must not depend on the
    // developer's own USER.
    command.env("USER", "tester");
    // A scratch config home under the scratch home itself: without this
    // the child inherits the developer's own XDG_CONFIG_HOME and can
    // write its unit file into the real config. Explicit per-run
    // overrides still win: they apply after.
    command.env("XDG_CONFIG_HOME", home.home().join(".config"));
    for (key, value) in extra_env {
        command.env(key, value);
    }
    match fail {
        Some(pattern) => {
            command.env("TS_SHIM_FAIL", pattern);
        }
        None => {
            command.env_remove("TS_SHIM_FAIL");
        }
    }
    command
}

/// Spawns a built [`Command`] with piped output and waits for it.
#[cfg(unix)]
fn spawn_and_wait(mut command: Command) -> Output {
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_for_exit(child)
}

/// A copy of the test target staged under `dir_name` in the scratch home,
/// so installs run from paths the shell and the service files escape.
#[cfg(unix)]
fn staged_binary(home: &ShimHome, dir_name: &str) -> PathBuf {
    let dir = home.home().join(dir_name);
    fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("ts");
    fs::copy(env!("CARGO_BIN_EXE_ts"), &binary).unwrap();
    binary
}

#[cfg(all(unix, target_os = "linux"))]
fn shim_unit_file(home: &ShimHome) -> PathBuf {
    home.home().join(".config/systemd/user/toka-search.service")
}

#[cfg(all(unix, target_os = "linux"))]
fn shim_legacy_unit_file(home: &ShimHome) -> PathBuf {
    home.home()
        .join(".config/systemd/user/toka-indexer.service")
}

#[cfg(all(unix, target_os = "macos"))]
fn shim_plist_file(home: &ShimHome) -> PathBuf {
    home.home().join("Library/LaunchAgents/app.toka.ts.plist")
}

#[cfg(unix)]
fn shim_link(home: &ShimHome) -> PathBuf {
    home.home().join(".local/bin/ts")
}

#[cfg(unix)]
#[test]
fn the_systemctl_shim_answers_absent_for_never_loaded_units() {
    // Pins the shim's never-loaded answers without a `ts` run: exit-4
    // `inactive` from `is-active`, an empty `ExecStart=` from `show` — the
    // words real systemctl uses for a unit it never loaded since systemd
    // 258.
    let dir = tempdir().unwrap();
    let shim = dir.path().join("systemctl");
    fs::write(&shim, SYSTEMCTL_SHIM).unwrap();
    let home = dir.path().join("home");
    let xdg = dir.path().join("xdg");
    let state = dir.path().join("state");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&xdg).unwrap();
    fs::create_dir_all(&state).unwrap();
    let log = dir.path().join("shim.log");
    let run = |args: &[&str]| {
        Command::new("sh")
            .arg(&shim)
            .args(args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("TS_SHIM_STATE", &state)
            .env("TS_SHIM_LOG", &log)
            .env_remove("TS_SHIM_FAIL")
            .output()
            .unwrap()
    };

    let probe = run(&["--user", "is-active", "toka-search.service"]);
    assert_eq!(probe.status.code(), Some(4));
    assert_eq!(String::from_utf8(probe.stdout).unwrap(), "inactive\n");

    let show = run(&["--user", "show", "-p", "ExecStart", "toka-search.service"]);
    assert_eq!(String::from_utf8(show.stdout).unwrap(), "ExecStart=\n");

    // A pinned word wins outright, and a unit file the manager would
    // load on demand reads as loaded-but-stopped.
    fs::write(state.join("word_toka-search.service"), "inaktiv\n").unwrap();
    let probe = run(&["--user", "is-active", "toka-search.service"]);
    assert_eq!(String::from_utf8(probe.stdout).unwrap(), "inaktiv\n");
    fs::remove_file(state.join("word_toka-search.service")).unwrap();

    let unit = xdg.join("systemd/user/toka-search.service");
    fs::create_dir_all(unit.parent().unwrap()).unwrap();
    fs::write(&unit, "[Unit]\n").unwrap();
    let probe = run(&["--user", "is-active", "toka-search.service"]);
    assert_eq!(probe.status.code(), Some(3));
    assert_eq!(String::from_utf8(probe.stdout).unwrap(), "inactive\n");
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn install_writes_enables_and_starts_the_systemd_unit() {
    let home = shim_home();

    let output = run_shimmed(&home, &["install"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let unit = fs::read_to_string(shim_unit_file(&home)).unwrap();
    assert!(
        unit.contains(&format!(
            "ExecStart=\"{}\" daemon run",
            env!("CARGO_BIN_EXE_ts")
        )),
        "{unit}"
    );
    for line in [
        "Description=Toka search indexer",
        "Type=exec",
        "Restart=on-failure",
        "RestartSec=2",
        "Nice=10",
        "IOSchedulingClass=idle",
        "WantedBy=default.target",
    ] {
        assert!(unit.contains(line), "{unit}");
    }
    let calls = home.calls();
    for verb in [
        "daemon-reload",
        "enable toka-search",
        "start toka-search",
        "enable-linger",
    ] {
        assert!(calls.contains(verb), "{calls}");
    }
    assert!(output.stderr.is_empty());
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn install_is_idempotent() {
    let home = shim_home();
    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));

    let output = run_shimmed(&home, &["install"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("already"), "{stdout}");
    assert!(!stdout.contains("wrote ~/"), "{stdout}");
    // Convergence checks only: nothing enabled, started, or reloaded twice.
    let calls = home.calls();
    assert_eq!(calls.matches("enable toka-search").count(), 1, "{calls}");
    assert_eq!(calls.matches("start toka-search").count(), 1, "{calls}");
    assert_eq!(calls.matches("daemon-reload").count(), 1, "{calls}");
    assert!(output.stderr.is_empty());
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn repeat_install_from_a_percent_dir_reports_already_installed() {
    // The unit escapes the executable's `%` as `%%`; the manager reports
    // the decoded path back, so a repeat install sees the same executable
    // — no takeover, no restart.
    let home = shim_home();
    let binary = staged_binary(&home, "100%");
    let install = || spawn_and_wait(shimmed_command(&home, &binary, &["install"], None, &[]));

    let output = install();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = install();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("taking over"), "{stdout}");
    assert!(stdout.contains("already"), "{stdout}");
    assert!(!stdout.contains("wrote ~/"), "{stdout}");
    let calls = home.calls();
    assert_eq!(calls.matches("start toka-search").count(), 1, "{calls}");
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn install_migrates_the_old_service() {
    let home = shim_home();
    let legacy = shim_legacy_unit_file(&home);
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    fs::write(&legacy, "[Unit]\n").unwrap();
    fs::write(home.state.join("active_toka-indexer.service"), "1").unwrap();
    fs::write(home.state.join("enabled_toka-indexer.service"), "1").unwrap();

    let output = run_shimmed(&home, &["install"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!legacy.exists());
    assert!(shim_unit_file(&home).is_file());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("stopped the old toka-indexer.service"),
        "{stdout}"
    );
    let calls = home.calls();
    assert!(calls.contains("stop toka-indexer.service"), "{calls}");
    assert!(calls.contains("disable toka-indexer.service"), "{calls}");
    let stop = calls.find("stop toka-indexer").unwrap();
    let start = calls.find("start toka-search").unwrap();
    assert!(stop < start, "{calls}");
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn install_respects_xdg_config_home() {
    let home = shim_home();
    let config = home
        .home()
        .join("xdg-config")
        .to_string_lossy()
        .into_owned();

    let output = run_shimmed_with_fail(
        &home,
        &["install"],
        None,
        &[("XDG_CONFIG_HOME", config.as_str())],
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        Path::new(&config)
            .join("systemd/user/toka-search.service")
            .is_file(),
        "the unit lands under XDG_CONFIG_HOME"
    );
    assert!(
        !shim_unit_file(&home).exists(),
        "nothing lands under ~/.config"
    );

    // Status finds it there too.
    let output = run_shimmed_with_fail(
        &home,
        &["status"],
        None,
        &[("XDG_CONFIG_HOME", config.as_str())],
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Service:  installed"), "{stdout}");
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn install_fails_when_the_manager_refuses_to_start() {
    let home = shim_home();

    let output = run_shimmed_with_fail(&home, &["install"], Some("start toka-search.service"), &[]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("ts install: systemctl"), "{stderr}");
    // The failure still prints what completed before it.
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("wrote ~/"), "{stdout}");
    assert!(stdout.contains("enabled toka-search.service"), "{stdout}");
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn fresh_install_with_never_loaded_units_starts_without_bogus_stops() {
    // The shim answers real systemctl's exit-4 `inactive` for units it
    // never loaded: a fresh install starts the service without stopping
    // the absent first.
    let home = shim_home();

    let output = run_shimmed(&home, &["install"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("started toka-search.service"), "{stdout}");
    assert!(!stdout.contains("restarted"), "{stdout}");
    assert!(!stdout.contains("stopped"), "{stdout}");
    let calls = home.calls();
    assert!(!calls.contains("stop"), "{calls}");
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn stop_with_an_unreadable_word_and_a_file_fails_closed() {
    // A pinned `unknown` — or a localized word — with a unit file
    // installed: a lingering service may still run there, so stop errors
    // instead of claiming convergence.
    let home = shim_home();
    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));

    for word in ["unknown", "inaktiv"] {
        fs::write(
            home.state.join("word_toka-search.service"),
            format!("{word}\n"),
        )
        .unwrap();

        let output = run_shimmed(&home, &["stop"]);

        assert_eq!(output.status.code(), Some(1), "{word}");
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("could not ask systemd"),
            "{word}"
        );
    }
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn uninstall_stops_and_removes_the_unit_idempotently() {
    let home = shim_home();
    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));

    let output = run_shimmed(&home, &["uninstall"]);

    assert_eq!(output.status.code(), Some(0));
    assert!(!shim_unit_file(&home).exists());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("stopped toka-search.service"), "{stdout}");
    assert!(stdout.contains("removed ~/"), "{stdout}");
    let calls = home.calls();
    assert!(calls.contains("stop toka-search.service"), "{calls}");
    assert!(calls.contains("disable toka-search.service"), "{calls}");

    let output = run_shimmed(&home, &["uninstall"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("toka-search.service is not installed"),
        "{stdout}"
    );
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn start_stop_and_restart_converge_with_honest_exit_codes() {
    let home = shim_home();

    let output = run_shimmed(&home, &["start"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("not installed"));
    let output = run_shimmed(&home, &["restart"]);
    assert_eq!(output.status.code(), Some(1));
    let output = run_shimmed(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("nothing to stop"),
        "stopping what was never installed succeeds"
    );

    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("already stopped"));
    let output = run_shimmed(&home, &["start"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["start"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("already running"));
    // Restart while stopped still ends up running.
    let output = run_shimmed(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["restart"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("already stopped"), "{stdout}");
    assert!(stdout.contains("started toka-search.service"), "{stdout}");
    // Restart while running stops first, then starts.
    let output = run_shimmed(&home, &["restart"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("stopped toka-search.service"), "{stdout}");
    assert!(stdout.contains("started toka-search.service"), "{stdout}");
}

#[cfg(all(unix, target_os = "linux"))]
#[test]
fn status_reports_the_systemd_service_matrix() {
    let home = shim_home();

    // Neither a unit nor a daemon: honestly not installed.
    let output = run_shimmed(&home, &["status"]);
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Service:  not installed\n"), "{stdout}");
    let output = run_shimmed(&home, &["status", "--json"]);
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["serviceInstalled"], false);
    assert!(document["serviceEnabled"].is_null());
    assert!(document["serviceRunning"].is_null());

    // Installed and running.
    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["status"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("Service:  installed, enabled, running\n"),
        "{stdout}"
    );
    let output = run_shimmed(&home, &["status", "--json"]);
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["serviceInstalled"], true);
    assert_eq!(document["serviceEnabled"], true);
    assert_eq!(document["serviceRunning"], true);

    // Installed but stopped.
    let output = run_shimmed(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["status"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("Service:  installed, enabled, stopped\n"),
        "{stdout}"
    );

    let output = run_shimmed(&home, &["uninstall"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["status"]);
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("Service:  not installed\n"));
}

#[cfg(all(unix, target_os = "macos"))]
#[test]
fn install_writes_and_bootstraps_the_agent() {
    let home = shim_home();

    let output = run_shimmed(&home, &["install"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plist = fs::read_to_string(shim_plist_file(&home)).unwrap();
    assert!(plist.contains("<string>app.toka.ts</string>"), "{plist}");
    assert!(
        plist.contains(&format!("<string>{}</string>", env!("CARGO_BIN_EXE_ts"))),
        "{plist}"
    );
    assert!(plist.contains("<string>daemon</string>"), "{plist}");
    assert!(plist.contains("<string>run</string>"), "{plist}");
    for key in ["KeepAlive", "RunAtLoad", "ProcessType", "LowPriorityIO"] {
        assert!(plist.contains(&format!("<key>{key}</key>")), "{plist}");
    }
    // launchd agents inherit a minimal PATH: the plist pins one that
    // covers both Homebrew prefixes so background thumbnails find ffmpeg.
    assert!(
        plist.contains("<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>"),
        "{plist}"
    );
    assert!(plist.contains("<string>Background</string>"), "{plist}");
    let calls = home.calls();
    assert!(calls.contains("bootstrap"), "{calls}");
    assert!(!calls.contains("systemctl"), "{calls}");
    assert!(output.stderr.is_empty());
}

#[cfg(all(unix, target_os = "macos"))]
#[test]
fn install_is_idempotent() {
    let home = shim_home();
    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));

    let output = run_shimmed(&home, &["install"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("already"), "{stdout}");
    assert!(!stdout.contains("wrote ~/"), "{stdout}");
    let calls = home.calls();
    assert_eq!(calls.matches("bootstrap").count(), 1, "{calls}");
    assert!(output.stderr.is_empty());
}

#[cfg(all(unix, target_os = "macos"))]
#[test]
fn install_without_a_reported_program_reports_already_running() {
    // Old `launchctl print` output names no program: with the plist
    // current and no marker set, the install takes the fast path — the
    // program line is never consulted.
    let home = shim_home();
    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));
    fs::remove_file(home.state.join("program")).unwrap();

    let output = run_shimmed(&home, &["install"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("already running"), "{stdout}");
    let calls = home.calls();
    assert_eq!(calls.matches("bootout").count(), 0, "{calls}");
    assert_eq!(calls.matches("bootstrap").count(), 1, "{calls}");
}

#[cfg(all(unix, target_os = "macos"))]
#[test]
fn repeat_install_from_an_ampersand_dir_stays_converged() {
    // The plist escapes the executable's `&` as `&amp;`; the manager
    // reports the decoded path back, so a repeat install sees the same
    // executable and never reloads an unchanged agent.
    let home = shim_home();
    let binary = staged_binary(&home, "A&B");
    let install = || spawn_and_wait(shimmed_command(&home, &binary, &["install"], None, &[]));

    let output = install();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(home.state.join("program")).unwrap(),
        binary.to_string_lossy().into_owned(),
        "the shim records the decoded loaded program"
    );

    let output = install();

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("taking over"), "{stdout}");
    assert!(stdout.contains("already running"), "{stdout}");
    assert!(!stdout.contains("wrote ~/"), "{stdout}");
    let calls = home.calls();
    assert_eq!(calls.matches("bootout").count(), 0, "{calls}");
    assert_eq!(calls.matches("bootstrap").count(), 1, "{calls}");
}

#[cfg(all(unix, target_os = "macos"))]
#[test]
fn retry_without_a_reported_program_completes_the_pending_reload() {
    // Old `launchctl print` output names no program, and the executable
    // never moves — only a pinned `XDG_DATA_HOME` changes the plist. A
    // bootout failure leaves the marker set; the program-less retry still
    // reloads, driven by the marker alone.
    let home = shim_home();
    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));
    let data = home.home().join("data-two").to_string_lossy().into_owned();

    let output = run_shimmed_with_fail(
        &home,
        &["install"],
        Some("bootout"),
        &[("XDG_DATA_HOME", data.as_str())],
    );
    assert_eq!(output.status.code(), Some(1));
    // Old launchctl from here on: the retry's `print` names no program.
    fs::remove_file(home.state.join("program")).unwrap();

    let output = run_shimmed_with_fail(
        &home,
        &["install"],
        None,
        &[("XDG_DATA_HOME", data.as_str())],
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("stopped app.toka.ts"), "{stdout}");
    assert!(stdout.contains("started app.toka.ts"), "{stdout}");
    assert!(!stdout.contains("already running"), "{stdout}");
    let plist = fs::read_to_string(shim_plist_file(&home)).unwrap();
    assert!(
        plist.contains(&format!("<string>{data}</string>")),
        "the retry pins data-two's absolute path, not the old value:\n{plist}"
    );
}

#[cfg(all(unix, target_os = "macos"))]
#[test]
fn install_failing_at_bootstrap_reports_the_bootout() {
    // The bootout succeeds, then the bootstrap fails: the partial report
    // names the stop instead of omitting it, and a retry completes the
    // pending reload.
    let home = shim_home();
    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));
    let data = home.home().join("data-two").to_string_lossy().into_owned();

    let output = run_shimmed_with_fail(
        &home,
        &["install"],
        Some("bootstrap"),
        &[("XDG_DATA_HOME", data.as_str())],
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("ts install: launchctl"), "{stderr}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("stopped app.toka.ts"), "{stdout}");

    let output = run_shimmed_with_fail(
        &home,
        &["install"],
        None,
        &[("XDG_DATA_HOME", data.as_str())],
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("started app.toka.ts"), "{stdout}");
}

#[cfg(all(unix, target_os = "macos"))]
#[test]
fn install_fails_when_the_manager_refuses_to_bootstrap() {
    let home = shim_home();

    let output = run_shimmed_with_fail(&home, &["install"], Some("bootstrap"), &[]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("ts install: launchctl"), "{stderr}");
    // The failure still prints what completed before it.
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("wrote ~/"), "{stdout}");
}

#[cfg(all(unix, target_os = "macos"))]
#[test]
fn uninstall_bootouts_and_removes_the_agent_idempotently() {
    let home = shim_home();
    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));

    let output = run_shimmed(&home, &["uninstall"]);

    assert_eq!(output.status.code(), Some(0));
    assert!(!shim_plist_file(&home).exists());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("stopped app.toka.ts"), "{stdout}");
    assert!(stdout.contains("removed ~/"), "{stdout}");
    assert!(home.calls().contains("bootout"), "{}", home.calls());

    let output = run_shimmed(&home, &["uninstall"]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("app.toka.ts is not installed"), "{stdout}");
}

#[cfg(all(unix, target_os = "macos"))]
#[test]
fn start_stop_and_restart_converge_with_honest_exit_codes() {
    let home = shim_home();

    let output = run_shimmed(&home, &["start"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("not installed"));
    let output = run_shimmed(&home, &["restart"]);
    assert_eq!(output.status.code(), Some(1));
    let output = run_shimmed(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("nothing to stop"),
        "stopping what was never installed succeeds"
    );

    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("already stopped"));
    let output = run_shimmed(&home, &["start"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["start"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("already running"));
    // Restart while stopped still ends up running.
    let output = run_shimmed(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["restart"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("already stopped"), "{stdout}");
    assert!(stdout.contains("started app.toka.ts"), "{stdout}");
    // Restart while running unloads first, then loads.
    let output = run_shimmed(&home, &["restart"]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("stopped app.toka.ts"), "{stdout}");
    assert!(stdout.contains("started app.toka.ts"), "{stdout}");
}

#[cfg(all(unix, target_os = "macos"))]
#[test]
fn status_reports_the_agent_matrix() {
    let home = shim_home();

    // Neither an agent nor a daemon: honestly not installed.
    let output = run_shimmed(&home, &["status"]);
    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Service:  not installed\n"), "{stdout}");
    let output = run_shimmed(&home, &["status", "--json"]);
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["serviceInstalled"], false);
    assert!(document["serviceEnabled"].is_null());
    assert!(document["serviceRunning"].is_null());

    // Installed and running.
    let output = run_shimmed(&home, &["install"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["status"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("Service:  installed, enabled, running\n"),
        "{stdout}"
    );
    let output = run_shimmed(&home, &["status", "--json"]);
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["serviceInstalled"], true);
    assert_eq!(document["serviceEnabled"], true);
    assert_eq!(document["serviceRunning"], true);

    // Installed but stopped: unloaded, so not enabled, with running
    // unknown — a hand-started daemon may still run there.
    let output = run_shimmed(&home, &["stop"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["status"]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("Service:  installed, disabled\n"),
        "{stdout}"
    );

    let output = run_shimmed(&home, &["uninstall"]);
    assert_eq!(output.status.code(), Some(0));
    let output = run_shimmed(&home, &["status"]);
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("Service:  not installed\n"));
}

#[cfg(unix)]
#[test]
fn install_with_link_links_and_uninstall_removes_it() {
    let home = shim_home();

    let output = run_shimmed(&home, &["install", "--link"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_link(shim_link(&home)).unwrap(),
        PathBuf::from(env!("CARGO_BIN_EXE_ts"))
    );
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("linked ~/.local/bin/ts"),);

    let output = run_shimmed(&home, &["install", "--link"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("already linked"),);

    let output = run_shimmed(&home, &["uninstall"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(fs::symlink_metadata(shim_link(&home)).is_err());
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("removed ~/.local/bin/ts"),);
}

#[cfg(unix)]
#[test]
fn service_commands_report_versioned_json() {
    let home = shim_home();

    let output = run_shimmed(&home, &["install", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["command"], "install");
    assert!(!document["actions"].as_array().unwrap().is_empty());

    for command in ["restart", "start", "stop"] {
        let output = run_shimmed(&home, &[command, "--json"]);
        assert_eq!(output.status.code(), Some(0), "{command}");
        let document: serde_json::Value =
            serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
        assert_eq!(document["schema"], 1, "{command}");
        assert_eq!(document["command"], command, "{command}");
    }

    let output = run_shimmed(&home, &["uninstall", "--json"]);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["command"], "uninstall");
}

#[cfg(unix)]
#[test]
fn uninstall_purge_removes_data_but_keeps_media() {
    let home = shim_home();
    // A real index, through the real command.
    let media = home.home().join("media");
    fs::create_dir_all(&media).unwrap();
    let clip = create_video(&media, 0);
    let media_argument = media.canonicalize().unwrap().to_string_lossy().into_owned();
    let output = run_shimmed(&home, &["folders", "add", &media_argument]);
    assert_eq!(output.status.code(), Some(0));
    #[cfg(target_os = "macos")]
    let index_dir = home.home().join("Library/Application Support/toka");
    #[cfg(not(target_os = "macos"))]
    let index_dir = home.scratch.data_home.join("toka");
    assert!(index_dir.join("index.sqlite3").is_file());
    // Logs and thumbnail cache markers where the child resolves them.
    #[cfg(target_os = "macos")]
    let logs_dir = home.home().join("Library/Logs/app.toka.desktop");
    #[cfg(not(target_os = "macos"))]
    let logs_dir = home.scratch.data_home.join("app.toka.desktop/logs");
    fs::create_dir_all(&logs_dir).unwrap();
    fs::write(logs_dir.join("search.log"), b"log").unwrap();
    let cache_dir = thumbnail_cache_dir(home.home());
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(cache_dir.join("thumb.jpg"), b"thumb").unwrap();
    // Foreign content inside the index directory: purge deletes only
    // known index files, so this always survives.
    let foreign = index_dir.join("my-media-clip.mp4");
    fs::write(&foreign, b"video").unwrap();

    let output = run_shimmed(&home, &["uninstall", "--purge-data"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !index_dir.join("index.sqlite3").exists(),
        "the index is purged"
    );
    assert!(foreign.is_file(), "foreign index-dir content survives");
    assert!(index_dir.is_dir(), "the index directory itself survives");
    assert!(!logs_dir.exists(), "the logs are purged");
    assert!(!cache_dir.exists(), "the thumbnail cache is purged");
    assert!(clip.is_file(), "media is never purged");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("removed ~/"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn uninstall_purge_refuses_a_directory_at_an_index_file_path() {
    // A directory sitting at an index-artifact path, holding media: the
    // purge refuses it with a clear error and removes nothing else
    // either — not even the legitimate index file beside it.
    let home = shim_home();
    #[cfg(target_os = "macos")]
    let index_dir = home.home().join("Library/Application Support/toka");
    #[cfg(not(target_os = "macos"))]
    let index_dir = home.scratch.data_home.join("toka");
    fs::create_dir_all(&index_dir).unwrap();
    let sibling = index_dir.join("index.sqlite3");
    fs::write(&sibling, b"db").unwrap();
    let artifact = index_dir.join("search-renames.json");
    fs::create_dir(&artifact).unwrap();
    let media = artifact.join("clip.mp4");
    fs::write(&media, b"video").unwrap();

    let output = run_shimmed(&home, &["uninstall", "--purge-data"]);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("refusing to remove"), "{stderr}");
    assert!(stderr.contains("search-renames.json"), "{stderr}");
    assert!(media.is_file(), "media inside survives");
    assert!(sibling.is_file(), "the index file survives too");
}

#[cfg(unix)]
#[test]
fn a_second_purge_reports_nothing_to_do() {
    // The purge's own lock check must not plant the legacy lock it checks:
    // through the real CLI, a second purge reports nothing to do instead
    // of deleting and reporting a file the purge created.
    let home = shim_home();

    let output = run_shimmed(&home, &["uninstall", "--purge-data"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = run_shimmed(&home, &["uninstall", "--purge-data"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("no index data to remove"), "{stdout}");
    assert!(!stdout.contains("daemon.lock"), "{stdout}");
}

#[cfg(unix)]
#[test]
fn uninstall_purge_removes_the_daemon_log() {
    let home = shim_home();
    #[cfg(target_os = "macos")]
    let log = home
        .home()
        .join("Library/Application Support/app.toka.desktop/logs/indexer.log");
    #[cfg(not(target_os = "macos"))]
    let log = home
        .scratch
        .data_home
        .join("app.toka.desktop/logs/indexer.log");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, b"log").unwrap();
    // A neighbor the file-only target must leave alone: on macOS the
    // daemon log sits outside the wholesale search-log folder, so only the
    // log itself goes.
    #[cfg(target_os = "macos")]
    let neighbor = log.parent().unwrap().join("other.log");
    #[cfg(target_os = "macos")]
    fs::write(&neighbor, b"other").unwrap();

    let output = run_shimmed(&home, &["uninstall", "--purge-data"]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!log.exists(), "the daemon log is purged");
    #[cfg(target_os = "macos")]
    assert!(neighbor.is_file(), "neighbors of the daemon log survive");
}

#[cfg(unix)]
#[test]
fn overlapping_installs_serialize_and_converge() {
    // Two installers at once: the second waits on the install lock, then
    // sees the first installer's converged state — exactly one writes the
    // service file, and the other reports it already up to date.
    let home = shim_home();
    let binary = Path::new(env!("CARGO_BIN_EXE_ts"));
    let first = shimmed_command(&home, binary, &["install"], None, &[]);
    let second = shimmed_command(&home, binary, &["install"], None, &[]);
    let (first, second) = thread::scope(|scope| {
        let running = scope.spawn(|| spawn_and_wait(first));
        let waiting = scope.spawn(|| spawn_and_wait(second));
        (running.join().unwrap(), waiting.join().unwrap())
    });

    assert_eq!(
        first.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        second.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let combined =
        String::from_utf8(first.stdout).unwrap() + &String::from_utf8(second.stdout).unwrap();
    assert_eq!(
        combined.matches("wrote ~/").count(),
        1,
        "exactly one installer writes:\n{combined}"
    );
    assert!(
        combined.contains("service file already up to date"),
        "the other sees converged state:\n{combined}"
    );
}

#[cfg(unix)]
#[test]
fn service_help_and_usage_touch_nothing() {
    let home = scratch_home();
    for (command, usage) in [
        ("install", "Usage: ts install [--link] [--json]"),
        ("uninstall", "Usage: ts uninstall [--purge-data] [--json]"),
        ("restart", "Usage: ts restart [--json]"),
        ("start", "Usage: ts start [--json]"),
        ("stop", "Usage: ts stop [--json]"),
    ] {
        let output = run_at(&home, &[command, "--help"]);
        assert_eq!(output.status.code(), Some(0), "{command}");
        assert!(
            String::from_utf8(output.stdout).unwrap().contains(usage),
            "{command}"
        );
        let output = run_at(&home, &[command, "--bogus"]);
        assert_eq!(output.status.code(), Some(2), "{command}");
        assert!(
            String::from_utf8(output.stderr).unwrap().contains(usage),
            "{command}"
        );
    }
}

#[cfg(unix)]
#[test]
fn the_daemon_lock_lives_in_the_runtime_dir() {
    let home = scratch_home();

    with_daemon(&home, || {
        assert!(
            wait_for_status(&home, Duration::from_secs(30), |document| {
                document["running"] == true
            }),
            "the daemon never started"
        );

        // The primary lock sits in the runtime dir, far from the reset's
        // reach; the legacy one stays beside the index for old
        // supervisors.
        let runtime = home.home.path().join("tmp/runtime");
        let locks = fs::read_dir(&runtime)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(locks.len(), 1, "{locks:?}");
        assert!(
            locks[0].to_string_lossy().starts_with("daemon-"),
            "{locks:?}"
        );
        let database = scratch_database(&home);
        assert!(database.with_file_name("daemon.lock").is_file());
        for entry in fs::read_dir(database.parent().unwrap()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with("daemon-"),
                "no primary lock beside the index: {name}"
            );
        }
    });
}
