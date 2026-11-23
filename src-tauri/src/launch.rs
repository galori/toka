use crate::addons::AddonSearchLaunch;
use std::{ffi::OsString, path::PathBuf};

pub const ADDON_SEARCH_FLAG: &str = "--toka-addon-search";

/// Whether this process should hand its GUI launch to a detached child and
/// exit: a release media, empty, or add-on search launch whose standard
/// input, output, and error are all terminals, i.e. Toka was started from an
/// interactive shell. CLI commands, debug builds (such as `tauri dev`), and
/// piped or headless launches stay in this process, so their output and exit
/// status still reach the caller.
#[cfg(any(
    test,
    all(target_os = "linux", not(feature = "e2e"), not(debug_assertions))
))]
pub fn should_detach_gui(
    arguments: &[OsString],
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
    stderr_is_terminal: bool,
) -> bool {
    let stays_foreground = crate::cli::is_command(arguments);
    !stays_foreground && stdin_is_terminal && stdout_is_terminal && stderr_is_terminal
}

/// Starts the same GUI launch as a session-detached child with no standard
/// streams and exits this process, so the shell that started Toka gets its
/// prompt back immediately and never sees the GUI toolkit's output. Only
/// called after [`should_detach_gui`] agrees.
#[cfg(all(target_os = "linux", not(feature = "e2e"), not(debug_assertions)))]
pub fn detach_gui(arguments: &[OsString]) -> ! {
    use std::os::unix::process::CommandExt;

    let spawn = || -> Result<(), String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("Toka could not find its own program: {error}"))?;
        let mut command = std::process::Command::new(&executable);
        command.args(arguments.iter().skip(1));
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: setsid is async-signal-safe; it detaches the child so it
        // survives this process and its terminal.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        command
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("Toka could not start {}: {error}", executable.display()))
    };

    match spawn() {
        Ok(()) => std::process::exit(0),
        Err(message) => {
            eprintln!("toka: {message}");
            std::process::exit(1);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchIntent {
    Empty,
    Media(PathBuf),
    AddonSearch(AddonSearchLaunch),
}

pub fn from_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<LaunchIntent, String> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if arguments
        .get(1)
        .is_some_and(|argument| argument == ADDON_SEARCH_FLAG)
    {
        if arguments.len() != 3 {
            return Err("The add-on search launch is incomplete.".into());
        }
        let encoded = arguments[2]
            .to_str()
            .ok_or_else(|| "The add-on search launch is not valid text.".to_owned())?;
        let search = serde_json::from_str(encoded)
            .map_err(|error| format!("The add-on search launch is invalid: {error}"))?;
        return Ok(LaunchIntent::AddonSearch(search));
    }
    Ok(match crate::playlist::from_arguments(arguments) {
        Some(path) => LaunchIntent::Media(path),
        None => LaunchIntent::Empty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{MediaType, SearchFields};

    fn arguments(arguments: impl IntoIterator<Item = String>) -> Vec<OsString> {
        arguments.into_iter().map(OsString::from).collect()
    }

    #[test]
    fn a_media_launch_from_a_terminal_hands_off_to_a_detached_child() {
        assert!(super::should_detach_gui(
            &arguments(["toka".into(), "sample1.mp4".into()]),
            true,
            true,
            true,
        ));
    }

    #[test]
    fn an_empty_launch_from_a_terminal_hands_off_to_a_detached_child() {
        assert!(super::should_detach_gui(
            &arguments(["toka".into()]),
            true,
            true,
            true,
        ));
    }

    #[test]
    fn an_addon_search_launch_from_a_terminal_hands_off_to_a_detached_child() {
        assert!(super::should_detach_gui(
            &arguments(["toka".into(), "--toka-addon-search".into(), "{}".into()]),
            true,
            true,
            true,
        ));
    }

    #[test]
    fn a_launch_with_any_piped_stream_stays_in_the_foreground() {
        let launch = arguments(["toka".into(), "sample1.mp4".into()]);
        for (stdin, stdout, stderr) in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
            (false, false, true),
            (false, true, false),
            (true, false, false),
        ] {
            assert!(
                !super::should_detach_gui(&launch, stdin, stdout, stderr),
                "stdin={stdin} stdout={stdout} stderr={stderr}"
            );
        }
    }

    #[test]
    fn cli_commands_stay_in_the_terminal_they_were_run_from() {
        for words in [
            vec!["toka", "search", "cats"],
            vec!["toka", "reset"],
            vec!["toka", "reset", "--yes"],
            vec!["toka", "restart"],
            vec!["toka", "--index-stats"],
            vec!["toka", "--help"],
            vec!["toka", "-h"],
            vec!["toka", "--version"],
            vec!["toka", "-v"],
        ] {
            let owned: Vec<String> = words.into_iter().map(str::to_owned).collect();
            assert!(
                !super::should_detach_gui(&arguments(owned.clone()), true, true, true),
                "{owned:?}"
            );
        }
    }

    #[test]
    fn a_piped_gui_launch_stays_in_the_foreground() {
        assert!(!super::should_detach_gui(
            &arguments(["toka".into(), "sample1.mp4".into()]),
            false,
            false,
            false,
        ));
    }

    #[test]
    fn addon_search_launch_round_trips_through_one_process_argument() {
        let search = AddonSearchLaunch {
            query: "@sometiktoker".into(),
            fields: SearchFields {
                tags: false,
                file_name: true,
                path: false,
            },
            media_type: MediaType::Videos,
            addon_id: "tiktok".into(),
            capture: "handle".into(),
            value: "@sometiktoker".into(),
        };
        let encoded = serde_json::to_string(&search).unwrap();

        assert_eq!(
            from_arguments(arguments([
                "toka".into(),
                ADDON_SEARCH_FLAG.into(),
                encoded,
            ]))
            .unwrap(),
            LaunchIntent::AddonSearch(search)
        );
    }
}
