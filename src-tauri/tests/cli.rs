// `toka` delegates every search, stats, and reset call to `ts`, resolved
// as the `ts` beside the `toka` under test — which `npm run rust:test`
// builds with the fixtures feature *before* this leg runs, so the
// fixture searches below answer. `TOKA_E2E_VIDEOS` reaches that `ts` as
// `TS_FIXTURE_VIDEOS` through the app's forwarding.
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

/// The `ts` beside the `toka` under test: what delegation resolves.
fn sibling_ts() -> PathBuf {
    Path::new(env!("CARGO_BIN_EXE_toka"))
        .parent()
        .unwrap()
        .join("ts")
}

fn run_toka(arguments: &[&str], videos: &[PathBuf]) -> Output {
    run_toka_with_provider(arguments, videos, None)
}

fn run_toka_with_ts_bin(arguments: &[&str], videos: &[PathBuf], ts_bin: &Path) -> Output {
    run_toka_with_provider_and_ts_bin(arguments, videos, None, Some(ts_bin))
}

fn run_toka_with_provider(
    arguments: &[&str],
    videos: &[PathBuf],
    provider: Option<&str>,
) -> Output {
    run_toka_with_provider_and_ts_bin(arguments, videos, provider, None)
}

fn run_toka_with_provider_and_ts_bin(
    arguments: &[&str],
    videos: &[PathBuf],
    provider: Option<&str>,
    ts_bin: Option<&Path>,
) -> Output {
    let home = tempdir().unwrap();
    let data_home = home.path().join("data");
    fs::create_dir_all(&data_home).unwrap();
    let cache_home = home.path().join("cache");
    fs::create_dir_all(&cache_home).unwrap();
    let tmp = home.path().join("tmp");
    fs::create_dir_all(&tmp).unwrap();
    let video_paths = std::env::join_paths(videos).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_toka"));
    command
        .args(arguments)
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", data_home)
        .env("XDG_CACHE_HOME", cache_home)
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("TMPDIR", &tmp)
        .env("TOKA_RUNTIME_DIR", home.path().join("runtime"))
        .env("TOKA_E2E_VIDEOS", video_paths)
        .env_remove("TOKA_SEARCH_PROVIDER");
    if let Some(provider) = provider {
        command.env("TOKA_SEARCH_PROVIDER", provider);
    }
    match ts_bin {
        Some(ts_bin) => {
            command.env("TS_BIN", ts_bin);
        }
        None => {
            command.env_remove("TS_BIN");
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
                "toka did not exit in time; stdout: {}; stderr: {}",
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

#[test]
fn search_prints_every_matching_path_and_exits_successfully() {
    require_fixture_ts();
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

    let output = run_toka(&["search", "family", "trip"], &videos);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    assert!(output.stderr.is_empty());
}

#[test]
fn search_without_a_query_prints_usage_and_exits_with_a_usage_error() {
    let output = run_toka(&["search"], &[]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Search needs a query.\nUsage: toka search <query>\n"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn search_help_prints_usage_and_exits_successfully() {
    let output = run_toka(&["search", "--help"], &[]);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "Usage: toka search <query>\n\nSearch Toka's indexed videos and images. Print one full path per result line.\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn search_with_no_matches_succeeds_without_output() {
    let output = run_toka(&["search", "missing"], &[]);

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn search_ignores_the_removed_provider_override() {
    let output = run_toka_with_provider(&["search", "family"], &[], Some("missing"));

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

/// Fails with guidance unless the sibling `ts` answers fixtures: running
/// this leg without the fixtures build behind it is a setup error, not a
/// product failure.
fn require_fixture_ts() {
    let home = tempdir().unwrap();
    let data_home = home.path().join("data");
    fs::create_dir_all(&data_home).unwrap();
    // A real file: `ts search` drops hits it cannot resolve, exactly as
    // the app's provider does.
    let marker = home.path().join("marker.mp4");
    fs::write(&marker, b"fixture video").unwrap();
    let binary = sibling_ts();
    let output = Command::new(&binary)
        .args(["search", "marker", "--json"])
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", &data_home)
        .env("TS_FIXTURE_VIDEOS", &marker)
        .output()
        .unwrap();
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("marker.mp4"),
        "the `ts` beside the test `toka` ({}) does not answer fixtures; build it with `cargo test -p ts --features fixtures` first (`npm run rust:test` orders this)",
        binary.display(),
    );
}

#[test]
fn search_delegates_through_the_sibling_ts() {
    require_fixture_ts();
    let directory = tempdir().unwrap();
    let video = create_video(directory.path(), 7);
    let videos = vec![video.clone()];

    let output = run_toka(&["search", "family"], &videos);

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", video.display())
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn search_honors_an_explicit_ts_bin() {
    require_fixture_ts();
    let directory = tempdir().unwrap();
    let video = create_video(directory.path(), 3);
    let videos = vec![video.clone()];

    let output = run_toka_with_ts_bin(&["search", "family"], &videos, &sibling_ts());

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("{}\n", video.display())
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn search_with_a_bogus_ts_bin_errors_clearly() {
    let output = run_toka_with_ts_bin(
        &["search", "family"],
        &[],
        Path::new("/nonexistent-ts-for-test"),
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "toka search: TS_BIN points at /nonexistent-ts-for-test, which does not exist.\n"
    );
}

#[test]
fn index_stats_reports_an_empty_index_through_ts() {
    require_fixture_ts();
    let output = run_toka(&["--index-stats"], &[]);

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Indexer:  idle"), "{stdout}");
    assert!(stdout.contains("Videos:   0"), "{stdout}");
    assert!(output.stderr.is_empty());
}

#[test]
fn reset_without_yes_lists_state_and_changes_nothing() {
    require_fixture_ts();
    let output = run_toka(&["reset"], &[]);

    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("Reset would remove these, if present:"),
        "{stdout}"
    );
    assert!(stdout.contains("index.sqlite3"), "{stdout}");
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("Run `toka reset --yes` to reset."));
}
