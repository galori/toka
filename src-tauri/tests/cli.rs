use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

fn run_toka(arguments: &[&str], videos: &[PathBuf]) -> Output {
    run_toka_with_provider(arguments, videos, None)
}

fn run_toka_with_provider(
    arguments: &[&str],
    videos: &[PathBuf],
    provider: Option<&str>,
) -> Output {
    let home = tempdir().unwrap();
    let data_home = home.path().join("data");
    fs::create_dir_all(&data_home).unwrap();
    let video_paths = std::env::join_paths(videos).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_toka"));
    command
        .args(arguments)
        .env("HOME", home.path())
        .env("XDG_DATA_HOME", data_home)
        .env("TOKA_E2E_VIDEOS", video_paths)
        .env_remove("TOKA_SEARCH_PROVIDER");
    if let Some(provider) = provider {
        command.env("TOKA_SEARCH_PROVIDER", provider);
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
fn search_provider_error_returns_a_failure_status() {
    let output = run_toka_with_provider(&["search", "family"], &[], Some("missing"));

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "toka search: Unknown Linux search provider: missing\n"
    );
    assert!(output.stdout.is_empty());
}
