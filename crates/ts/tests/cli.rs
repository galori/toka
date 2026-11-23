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

    run_ts_with_home(arguments, home.path(), &data_home, fixture_videos)
}

fn run_ts_with_home(
    arguments: &[&str],
    home: &Path,
    data_home: &Path,
    fixture_videos: Option<OsString>,
) -> Output {
    // A scratch temp space too, so even cache reads cannot escape the
    // test's directories.
    let scratch = home.join("tmp");
    fs::create_dir_all(&scratch).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_ts"));
    command
        .args(arguments)
        .env("HOME", home)
        .env("XDG_DATA_HOME", data_home)
        .env("TMPDIR", &scratch);
    match fixture_videos {
        Some(videos) => {
            command.env("TS_FIXTURE_VIDEOS", videos);
        }
        None => {
            command.env_remove("TS_FIXTURE_VIDEOS");
        }
    }

    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    wait_for_exit(child)
}

fn wait_for_exit(mut child: Child) -> Output {
    let started = Instant::now();
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if started.elapsed() > Duration::from_secs(10) {
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

    let output = run_ts_with_home(&["search", "family"], home.path(), &data_home, None);
    assert_eq!(output.status.code(), Some(0));
    let mut expected = [first.display().to_string(), second.display().to_string()];
    expected.sort();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        expected.join("\n") + "\n"
    );
    assert!(output.stderr.is_empty());

    let output = run_ts_with_home(&["stats", "--json"], home.path(), &data_home, None);
    assert_eq!(output.status.code(), Some(0));
    let document: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(document["schema"], 1);
    assert_eq!(document["videos"], 2);
    assert_eq!(document["folders"].as_array().unwrap().len(), 1);
    assert!(output.stderr.is_empty());

    let output = run_ts_with_home(&["folders", "list"], home.path(), &data_home, None);
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
        vec!["search", "family", "--json"],
        vec!["search", "family", "--verbose"],
        vec!["stats"],
        vec!["stats", "--json"],
        vec!["folders", "list"],
        vec!["folders", "list", "--json"],
    ] {
        let output = run_ts_with_home(&arguments, home.path(), &data_home, None);
        assert_eq!(output.status.code(), Some(0), "{arguments:?}");
    }
    assert!(files_under(home.path()).is_empty());
}

#[cfg(not(feature = "fixtures"))]
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
