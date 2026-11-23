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

fn run_ts_with_home(
    arguments: &[&str],
    home: &Path,
    data_home: &Path,
    fixture_videos: Option<OsString>,
    cwd: Option<&Path>,
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

    let cache = home
        .home
        .path()
        .join("tmp")
        .join(toka_search::config::THUMBNAIL_CACHE_DIR_NAME);
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
    let cache = home
        .home
        .path()
        .join("tmp")
        .join(toka_search::config::THUMBNAIL_CACHE_DIR_NAME);
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
