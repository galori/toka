use crate::ts_client::{self, TsOutput};
use std::{
    ffi::{OsStr, OsString},
    io::Write,
};
use toka_search::{
    index_stats,
    reset::{ResetPlan, ResetTarget},
    thumbnails,
};

const SEARCH_USAGE: &str = "Usage: toka search <query>";
const RESET_USAGE: &str = "Usage: toka reset --yes";
const GENERAL_HELP: &str = "Usage: toka <command> [arguments]\n       toka [path]\n\nCommands:\n  search <query>  Search indexed media and print file paths\n  reset --yes     Remove Toka's search folders, index, caches, and logs\n  restart         Restart this Toka and its indexer\n\nRun with a folder, video, image, or playlist to play it instead of opening an empty search.\n\nOptions:\n  -h, --help      Show this help\n  -v, --version   Show the Toka version and Git SHA\n  --index-stats   Show what the indexer has found and whether it is running";
const RESET_HELP: &str = "Usage: toka reset --yes\n\nPut Toka back to a first-run state. Removes every search folder, the search index and its caches, thumbnail and other caches, and Toka's log files.\nYour media files are not touched. Without --yes, lists what would be removed and changes nothing.";
const INDEX_STATS_HELP: &str = "Usage: toka --index-stats\n\nShow whether the indexer is active, the indexer service state, the number of videos and images indexed, and each search folder's status and file counts. Changes nothing.";
const SEARCH_HELP: &str =
    "Usage: toka search <query>\n\nSearch Toka's indexed videos and images. Print one full path per result line.";

/// The build version is the git-derived `0.1.1+482.a1b2c3d.branch` that build
/// scripts pass as `VITE_APP_VERSION`; a plain `cargo build` has none and falls
/// back to the manifest version.
fn build_version() -> &'static str {
    pick_version(option_env!("VITE_APP_VERSION"))
}

fn pick_version(build_version: Option<&'static str>) -> &'static str {
    build_version
        .filter(|version| !version.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// The version line also shows the same Git SHA as the launch screen. The build
/// script hands it over as `TOKA_GIT_SHA`; a build with no provenance has none,
/// so it reads "development" exactly as the frontend does.
fn version_text(version: &str, git_sha: Option<&str>) -> String {
    let sha = git_sha
        .filter(|sha| !sha.is_empty())
        .unwrap_or("development");
    format!("toka {version} (Git SHA {sha})")
}

pub fn is_command(arguments: &[OsString]) -> bool {
    arguments.get(1).is_some_and(|argument| {
        argument == OsStr::new("search")
            || argument == OsStr::new("reset")
            || argument == OsStr::new("restart")
            || argument == OsStr::new("--index-stats")
            || argument == OsStr::new("--help")
            || argument == OsStr::new("-h")
            || argument == OsStr::new("--version")
            || argument == OsStr::new("-v")
    })
}

pub fn run_command(
    arguments: &[OsString],
    run_ts: impl FnMut(&[OsString]) -> Result<TsOutput, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let Some(command) = arguments.get(1) else {
        let _ = writeln!(stderr, "{GENERAL_HELP}");
        return 2;
    };

    if command == OsStr::new("--help") || command == OsStr::new("-h") {
        return print_text(GENERAL_HELP, stdout, stderr);
    }

    if command == OsStr::new("--version") || command == OsStr::new("-v") {
        return print_text(
            &version_text(build_version(), option_env!("TOKA_GIT_SHA")),
            stdout,
            stderr,
        );
    }

    if command == OsStr::new("--indexer") {
        let _ = writeln!(
            stderr,
            "`toka --indexer` was replaced by `ts daemon run`. Run `ts install` to set up the indexer service."
        );
        return 2;
    }

    if command == OsStr::new("--index-stats") {
        return run_index_stats_command(&arguments[2..], run_ts, stdout, stderr);
    }

    if command == OsStr::new("reset") {
        return run_reset_command(&arguments[2..], run_ts, stdout, stderr);
    }

    if command == OsStr::new("restart") {
        return crate::restart::run(&arguments[2..], run_ts, stdout, stderr);
    }

    if command != OsStr::new("search") {
        let _ = writeln!(stderr, "{GENERAL_HELP}");
        return 2;
    }

    run_search_command(&arguments[2..], run_ts, stdout, stderr)
}

/// `toka --index-stats`: the same report as ever — numbers from `ts
/// stats --json`, rendered through the same human path.
fn run_index_stats_command(
    arguments: &[OsString],
    mut run_ts: impl FnMut(&[OsString]) -> Result<TsOutput, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(INDEX_STATS_HELP, stdout, stderr);
    }
    if !arguments.is_empty() {
        let _ = writeln!(stderr, "Usage: toka --index-stats");
        return 2;
    }
    let args = [OsString::from("stats"), OsString::from("--json")];
    let output = match run_ts(&args) {
        Ok(output) => output,
        Err(error) => {
            let _ = writeln!(stderr, "toka --index-stats: {error}");
            return 1;
        }
    };
    let stats = match ts_client::stats_from_output(&output) {
        Ok(stats) => stats,
        Err(error) => {
            let _ = writeln!(stderr, "toka --index-stats: {error}");
            return 1;
        }
    };
    let report = index_stats::render(&stats);
    match write!(stdout, "{report}").and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "toka --index-stats: could not write: {error}");
            1
        }
    }
}

/// `toka search`: the query words run through `ts search`, whose
/// human output is the same sorted paths this command always printed.
fn run_search_command(
    arguments: &[OsString],
    mut run_ts: impl FnMut(&[OsString]) -> Result<TsOutput, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(SEARCH_HELP, stdout, stderr);
    }

    if let Err(message) = query_from_arguments(arguments) {
        let _ = writeln!(stderr, "{message}\n{SEARCH_USAGE}");
        return 2;
    }

    // After `--` so a query starting with a dash stays a query.
    let mut ts_args = vec![OsString::from("search"), OsString::from("--")];
    ts_args.extend(arguments.iter().cloned());
    let output = match run_ts(&ts_args) {
        Ok(output) => output,
        Err(error) => {
            let _ = writeln!(stderr, "toka search: {error}");
            return 1;
        }
    };
    match output.status {
        0 => {
            if let Err(error) = stdout
                .write_all(&output.stdout)
                .and_then(|()| stdout.flush())
            {
                let _ = writeln!(stderr, "toka search: could not write results: {error}");
                return 1;
            }
            0
        }
        1 => {
            let _ = writeln!(
                stderr,
                "toka search: {}",
                ts_client::command_error("search", &output)
            );
            1
        }
        // The query was validated above and `search` never exits 3, so
        // anything else is a signal kill (status -1) or a future exit —
        // reported like any failure, never silent.
        _ => {
            let _ = writeln!(
                stderr,
                "toka search: {}",
                ts_client::command_error("search", &output)
            );
            1
        }
    }
}

/// `toka reset`: the search state through `ts reset`, plus the app's own
/// web cache `ts` does not own. Same flags, same preview shape, same
/// fail-closed stop — the preview lists, `--yes` deletes.
fn run_reset_command(
    arguments: &[OsString],
    mut run_ts: impl FnMut(&[OsString]) -> Result<TsOutput, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1
        && (arguments[0] == OsStr::new("--help") || arguments[0] == OsStr::new("-h"))
    {
        return print_text(RESET_HELP, stdout, stderr);
    }
    let confirmed = arguments.len() == 1
        && (arguments[0] == OsStr::new("--yes") || arguments[0] == OsStr::new("-y"));
    if !arguments.is_empty() && !confirmed {
        let _ = writeln!(
            stderr,
            "Reset does not take those arguments.\n{RESET_USAGE}"
        );
        return 2;
    }

    if !confirmed {
        return run_reset_preview(&mut run_ts, stdout, stderr);
    }
    run_confirmed_reset(&mut run_ts, stdout, stderr)
}

fn run_reset_preview(
    run_ts: &mut impl FnMut(&[OsString]) -> Result<TsOutput, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let args = [OsString::from("reset"), OsString::from("--json")];
    let output = match run_ts(&args) {
        Ok(output) => output,
        Err(error) => {
            let _ = writeln!(stderr, "toka reset: {error}");
            return 1;
        }
    };
    let preview = match ts_client::reset_preview_from_output(&output) {
        Ok(preview) => preview,
        Err(error) => {
            let _ = writeln!(stderr, "toka reset: {error}");
            return 1;
        }
    };
    let _ = writeln!(stdout, "Reset would remove these, if present:");
    for target in &preview_targets(&preview.targets) {
        let _ = writeln!(stdout, "  {}", target.path.display());
    }
    let _ = writeln!(
        stderr,
        "Nothing was changed. Run `toka reset --yes` to reset."
    );
    2
}

/// The preview list: `ts`'s targets with the app's own cache spliced in
/// where it always sat — right after the thumbnail cache — then
/// collapsed exactly as one reset plan.
fn preview_targets(targets: &[std::path::PathBuf]) -> Vec<ResetTarget> {
    let mut listed: Vec<ResetTarget> = targets.iter().cloned().map(ResetTarget::dir).collect();
    if let Some(cache) = app_cache_dir() {
        let target = ResetTarget::dir(cache);
        match listed
            .iter()
            .position(|listed| listed.path == thumbnails::cache_output_root())
        {
            Some(index) => listed.insert(index + 1, target),
            None => listed.push(target),
        }
    }
    // Kinds are placeholders for display: the collapse only compares
    // paths, and `ts` removes its own targets with their real kinds.
    ResetPlan::new(listed).targets
}

/// The app's own web cache: the one reset target `ts` does not own.
fn app_cache_dir() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|cache| cache.join("app.toka.desktop"))
}

fn run_confirmed_reset(
    run_ts: &mut impl FnMut(&[OsString]) -> Result<TsOutput, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    let args = [
        OsString::from("reset"),
        OsString::from("--yes"),
        OsString::from("--json"),
    ];
    let output = match run_ts(&args) {
        Ok(output) => output,
        Err(error) => {
            let _ = writeln!(stderr, "toka reset: {error}");
            return 1;
        }
    };
    let applied = match ts_client::reset_applied_from_output(&output) {
        Ok(applied) => applied,
        Err(error) => {
            let _ = writeln!(stderr, "toka reset: {error}");
            return 1;
        }
    };
    // Fail closed exactly as one plan: when the stop failed, `ts`
    // removed nothing — so the app cache stays too. A preflight refusal
    // reads the same: nothing removed with failures means the plan never
    // ran, and the app cache is part of that refused plan. A partial
    // removal already ran, so the app cache still goes, as the old
    // sequential plan continued past mid-plan failures.
    if !applied.indexer_stopped || (applied.removed.is_empty() && !applied.failures.is_empty()) {
        for failure in &applied.failures {
            let _ = writeln!(stderr, "toka reset: {failure}");
        }
        return 1;
    }
    let mut removed = applied.removed;
    let mut failures = applied.failures;
    if let Some(cache) = app_cache_dir() {
        let report = toka_search::reset::apply(&ResetPlan::new(vec![ResetTarget::dir(cache)]));
        removed.extend(report.removed);
        failures.extend(report.failures);
    }
    for path in &removed {
        let _ = writeln!(stdout, "removed {}", path.display());
    }
    for failure in &failures {
        let _ = writeln!(stderr, "toka reset: {failure}");
    }
    if !failures.is_empty() {
        return 1;
    }
    let _ = writeln!(stdout, "Toka was reset.");
    0
}

fn print_text(help: &str, stdout: &mut impl Write, stderr: &mut impl Write) -> i32 {
    if let Err(error) = writeln!(stdout, "{help}").and_then(|()| stdout.flush()) {
        let _ = writeln!(stderr, "toka: could not write output: {error}");
        return 1;
    }
    0
}

fn query_from_arguments(arguments: &[OsString]) -> Result<String, &'static str> {
    if arguments.is_empty() {
        return Err("Search needs a query.");
    }
    let words = arguments
        .iter()
        .map(|argument| argument.to_str().ok_or("Search terms must be valid text."))
        .collect::<Result<Vec<_>, _>>()?;
    let query = words.join(" ");
    if query.trim().is_empty() {
        return Err("Search needs a query.");
    }
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::run_command;
    use crate::ts_client::TsOutput;
    use std::ffi::OsString;

    fn no_ts(_args: &[OsString]) -> Result<TsOutput, String> {
        panic!("this path must not spawn ts")
    }

    fn ts_output(status: i32, stdout: &str, stderr: &str) -> TsOutput {
        TsOutput {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
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
    fn version_and_help_flags_print_without_spawning_anything() {
        for (flag, is_version) in [
            ("-v", true),
            ("--version", true),
            ("-h", false),
            ("--help", false),
        ] {
            let args = ["toka", flag].map(OsString::from).to_vec();
            assert!(super::is_command(&args), "{flag}");
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(&args, no_ts, &mut stdout, &mut stderr);

            assert_eq!(status, 0, "{flag}");
            let output = String::from_utf8(stdout).unwrap();
            if is_version {
                // The SHA is whatever the build recorded, so check the shape, and
                // that a recorded SHA really reaches the output.
                let prefix = format!("toka {} (Git SHA ", super::build_version());
                let sha = output
                    .strip_prefix(&prefix)
                    .and_then(|rest| rest.strip_suffix(")\n"))
                    .unwrap_or_else(|| panic!("unexpected version line: {output}"));
                assert_eq!(
                    Some(sha),
                    option_env!("TOKA_GIT_SHA").or(Some("development"))
                );
            } else {
                assert!(output.contains("--version"), "{output}");
            }
            assert!(stderr.is_empty());
        }
    }

    #[test]
    fn version_text_includes_the_git_sha_or_development() {
        assert_eq!(
            super::version_text("0.1.1+5.abc", Some("abc123")),
            "toka 0.1.1+5.abc (Git SHA abc123)"
        );
        assert_eq!(
            super::version_text("0.1.1", None),
            "toka 0.1.1 (Git SHA development)"
        );
        assert_eq!(
            super::version_text("0.1.1", Some("")),
            super::version_text("0.1.1", None)
        );
    }

    #[test]
    fn indexer_flag_explains_the_move() {
        let args = ["toka", "--indexer"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(&args, no_ts, &mut stdout, &mut stderr);

        assert_eq!(status, 2);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "`toka --indexer` was replaced by `ts daemon run`. Run `ts install` to set up the indexer service.\n"
        );
    }

    #[test]
    fn search_help_does_not_spawn_ts() {
        let args = ["toka", "search", "--help"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(&args, no_ts, &mut stdout, &mut stderr);

        assert_eq!(status, 0);
        assert!(String::from_utf8(stdout)
            .unwrap()
            .contains("Usage: toka search <query>"));
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_without_a_query_prints_usage_without_spawning() {
        for words in [vec!["toka", "search"], vec!["toka", "search", "  "]] {
            let args = words.into_iter().map(OsString::from).collect::<Vec<_>>();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let status = run_command(&args, no_ts, &mut stdout, &mut stderr);

            assert_eq!(status, 2);
            assert!(stdout.is_empty());
            assert!(String::from_utf8(stderr)
                .unwrap()
                .contains("Usage: toka search <query>"));
        }
    }

    #[test]
    fn search_passes_words_after_the_separator_and_prints_ts_output() {
        let args = ["toka", "search", "family", "video"]
            .map(OsString::from)
            .to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            |args: &[OsString]| {
                assert_eq!(
                    args,
                    &[
                        OsString::from("search"),
                        OsString::from("--"),
                        OsString::from("family"),
                        OsString::from("video"),
                    ]
                );
                Ok(ts_output(0, "/media/a.mp4\n/media/b.mp4\n", ""))
            },
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            "/media/a.mp4\n/media/b.mp4\n"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn search_ts_failure_reports_with_the_toka_prefix() {
        let args = ["toka", "search", "family"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            |_| Ok(ts_output(1, "", "ts search: no index to search\n")),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "toka search: no index to search\n"
        );
    }

    #[test]
    fn search_reports_a_signal_killed_ts_instead_of_panicking() {
        // A signal leaves `ts` with no exit code (`run` reports status
        // -1): the search reports it like any failure, even in debug
        // builds where an assertion used to panic on it.
        let args = ["toka", "search", "family"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            |_| Ok(ts_output(-1, "", "")),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "toka search: ts search failed.\n"
        );
    }

    #[test]
    fn search_ts_spawn_error_returns_a_failure_status() {
        let args = ["toka", "search", "family"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            |_| Err("Toka could not find the `ts` search helper.".to_owned()),
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "toka search: Toka could not find the `ts` search helper.\n"
        );
    }

    #[test]
    fn index_stats_renders_the_ts_numbers() {
        let args = vec![OsString::from("toka"), OsString::from("--index-stats")];
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());

        let code = run_command(
            &args,
            |args: &[OsString]| {
                assert_eq!(args, &[OsString::from("stats"), OsString::from("--json")]);
                Ok(ts_output(
                    0,
                    r#"{"schema": 1, "revision": 1, "videos": 4, "images": 1, "serviceRunning": true, "folders": [
                        {"path": "/media", "state": "ready", "message": null, "scannedFiles": 0, "videos": 4, "images": 1}
                    ]}"#,
                    "",
                ))
            },
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(code, 0);
        let text = String::from_utf8(stdout).unwrap();
        assert!(
            text.contains("Videos:   4") && text.contains("/media"),
            "{text}"
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn index_stats_failure_and_extra_arguments_are_errors() {
        let args = vec![OsString::from("toka"), OsString::from("--index-stats")];
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = run_command(
            &args,
            |_| Ok(ts_output(1, "", "ts stats: no index\n")),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(code, 1);
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "toka --index-stats: no index\n"
        );
        assert!(stdout.is_empty());

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = vec![
            OsString::from("toka"),
            OsString::from("--index-stats"),
            OsString::from("extra"),
        ];
        let code = run_command(&args, no_ts, &mut stdout, &mut stderr);
        assert_eq!(code, 2);

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let args = vec![
            OsString::from("toka"),
            OsString::from("--index-stats"),
            OsString::from("--help"),
        ];
        let code = run_command(&args, no_ts, &mut stdout, &mut stderr);
        assert_eq!(code, 0);
        assert!(String::from_utf8(stdout)
            .unwrap()
            .contains("Usage: toka --index-stats"));
        assert!(stderr.is_empty());
    }

    /// Redirects the cache dir at scratch, so reset tests name a
    /// throwaway app cache instead of the developer's own. `HOME` goes
    /// along because `dirs::cache_dir()` on macOS is
    /// `$HOME/Library/Caches`, ignoring `XDG_CACHE_HOME` — without it
    /// these tests would create and recursively delete the developer's
    /// real `~/Library/Caches/app.toka.desktop`. `TMPDIR` stays alone:
    /// `tempfile` resolves it process-wide, so redirecting it here
    /// would break parallel tests' tempdirs.
    fn redirect_reset_dirs(directory: &tempfile::TempDir) -> crate::ts_client::test_env::EnvGuard {
        let mut env = crate::ts_client::test_env::EnvGuard::lock();
        env.set("XDG_CACHE_HOME", directory.path().join("cache").as_os_str());
        env.set("HOME", directory.path().as_os_str());
        env
    }

    /// The app cache a reset targets, proven to sit under the test's own
    /// temp dir before anything is allowed to delete it.
    fn reset_app_cache_under(directory: &tempfile::TempDir) -> std::path::PathBuf {
        let cache = super::app_cache_dir().unwrap();
        assert!(
            cache.starts_with(directory.path()),
            "reset would touch {} outside {}",
            cache.display(),
            directory.path().display()
        );
        cache
    }

    #[test]
    fn reset_without_yes_lists_ts_targets_and_the_app_cache() {
        let directory = tempfile::tempdir().unwrap();
        let _env = redirect_reset_dirs(&directory);
        let thumb_root = toka_search::thumbnails::cache_output_root();
        let app_cache = reset_app_cache_under(&directory);
        // The app cache exists but must survive the preview.
        std::fs::create_dir_all(&app_cache).unwrap();
        let args = ["toka", "reset"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            |args: &[OsString]| {
                assert_eq!(args, &[OsString::from("reset"), OsString::from("--json")]);
                Ok(ts_output(
                    0,
                    &serde_json::json!({
                        "schema": 1,
                        "targets": [
                            "/state/index.sqlite3",
                            thumb_root.to_string_lossy(),
                            "/logs/search.log",
                        ],
                    })
                    .to_string(),
                    "",
                ))
            },
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 2);
        // The app cache splices in right after the thumbnail cache, where
        // the in-process plan always listed it.
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!(
                "Reset would remove these, if present:\n  /state/index.sqlite3\n  {}\n  {}\n  /logs/search.log\n",
                thumb_root.display(),
                app_cache.display(),
            )
        );
        assert!(String::from_utf8(stderr).unwrap().contains("--yes"));
        assert!(app_cache.is_dir(), "the preview removes nothing");
    }

    #[test]
    fn reset_with_yes_removes_ts_state_and_the_app_cache() {
        let directory = tempfile::tempdir().unwrap();
        let _env = redirect_reset_dirs(&directory);
        let app_cache = reset_app_cache_under(&directory);
        std::fs::create_dir_all(app_cache.join("webview")).unwrap();
        let args = ["toka", "reset", "--yes"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            |args: &[OsString]| {
                assert_eq!(
                    args,
                    &[
                        OsString::from("reset"),
                        OsString::from("--yes"),
                        OsString::from("--json"),
                    ]
                );
                Ok(ts_output(
                    0,
                    r#"{"schema": 1, "indexerStopped": true, "removed": ["/state/index.sqlite3"], "failures": []}"#,
                    "",
                ))
            },
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 0);
        assert!(!app_cache.exists(), "the app cache is removed");
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!(
                "removed /state/index.sqlite3\nremoved {}\nToka was reset.\n",
                app_cache.display()
            )
        );
        assert!(stderr.is_empty());
    }

    #[test]
    fn reset_with_yes_fails_closed_when_the_stop_failed() {
        let directory = tempfile::tempdir().unwrap();
        let _env = redirect_reset_dirs(&directory);
        let app_cache = reset_app_cache_under(&directory);
        std::fs::create_dir_all(&app_cache).unwrap();
        let args = ["toka", "reset", "--yes"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            |_| {
                Ok(ts_output(
                    1,
                    r#"{"schema": 1, "indexerStopped": false, "removed": [], "failures": ["refusing to reset while the indexer may be running."]}"#,
                    "ts reset: refusing to reset while the indexer may be running.\n",
                ))
            },
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "toka reset: refusing to reset while the indexer may be running.\n"
        );
        assert!(
            app_cache.is_dir(),
            "a failed stop removes nothing, not even the app cache"
        );
    }

    #[test]
    fn reset_with_yes_keeps_the_app_cache_on_a_preflight_refusal() {
        // `ts` refused before removing anything (a file-only target
        // occupied by a directory): the app cache is part of that same
        // refused plan, so it stays too.
        let directory = tempfile::tempdir().unwrap();
        let _env = redirect_reset_dirs(&directory);
        let app_cache = reset_app_cache_under(&directory);
        std::fs::create_dir_all(&app_cache).unwrap();
        let args = ["toka", "reset", "--yes"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            |_| {
                Ok(ts_output(
                    1,
                    r#"{"schema": 1, "indexerStopped": true, "removed": [], "failures": ["refusing to remove /state/index.sqlite3: a directory occupies this file's path"]}"#,
                    "ts reset: refusing to remove /state/index.sqlite3: a directory occupies this file's path\n",
                ))
            },
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "toka reset: refusing to remove /state/index.sqlite3: a directory occupies this file's path\n"
        );
        assert!(
            app_cache.is_dir(),
            "a refused reset removes nothing, not even the app cache"
        );
    }

    #[test]
    fn reset_with_yes_still_clears_the_app_cache_after_a_partial_removal() {
        // `ts` removed some targets before a failure: the reset already
        // ran, so the app cache goes too, exactly as the old sequential
        // plan continued past mid-plan failures.
        let directory = tempfile::tempdir().unwrap();
        let _env = redirect_reset_dirs(&directory);
        let app_cache = reset_app_cache_under(&directory);
        std::fs::create_dir_all(app_cache.join("webview")).unwrap();
        let args = ["toka", "reset", "--yes"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let status = run_command(
            &args,
            |_| {
                Ok(ts_output(
                    1,
                    r#"{"schema": 1, "indexerStopped": true, "removed": ["/state/index.sqlite3"], "failures": ["could not remove /logs/search.log: permission denied"]}"#,
                    "ts reset: could not remove /logs/search.log: permission denied\n",
                ))
            },
            &mut stdout,
            &mut stderr,
        );

        assert_eq!(status, 1);
        assert!(
            !app_cache.exists(),
            "a partial reset still clears the app cache"
        );
        assert_eq!(
            String::from_utf8(stdout).unwrap(),
            format!(
                "removed /state/index.sqlite3\nremoved {}\n",
                app_cache.display()
            )
        );
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "toka reset: could not remove /logs/search.log: permission denied\n"
        );
    }

    #[test]
    fn reset_help_and_bad_arguments_never_spawn_ts() {
        for (extra, status) in [("--help", 0), ("--force", 2)] {
            let args = ["toka", "reset", extra].map(OsString::from).to_vec();
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();

            let code = run_command(&args, no_ts, &mut stdout, &mut stderr);

            assert_eq!(code, status);
        }
    }

    #[test]
    fn restart_rejects_bad_arguments_without_spawning() {
        let args = ["toka", "restart", "extra"].map(OsString::from).to_vec();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let code = run_command(&args, no_ts, &mut stdout, &mut stderr);

        assert_eq!(code, 2);
        assert!(stdout.is_empty());
        assert!(String::from_utf8(stderr)
            .unwrap()
            .contains("Usage: toka restart"));
    }
}
