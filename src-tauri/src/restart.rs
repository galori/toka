//! `toka restart`: restart this Toka build's app and its indexer.
//!
//! A worktree build and the installed build can run side by side. App
//! instances are told apart by the executable they were started from, so
//! the command only ever touches app processes that belong to the
//! executable that was called — but the indexer is the one `ts` service
//! for the user, restarted with `ts restart` however this Toka was built.

#[cfg(any(target_os = "linux", test))]
use crate::ts_client;
use crate::ts_client::TsOutput;
#[cfg(any(target_os = "linux", test))]
use std::path::PathBuf;
use std::{ffi::OsString, io::Write};
#[cfg(target_os = "linux")]
use toka_search::service::same_executable;

#[cfg(target_os = "linux")]
use std::{fs, path::Path, process::Command, thread, time::Duration};

/// Everything `restart` needs from the machine, so the decisions can be tested.
#[cfg(any(target_os = "linux", test))]
pub trait Host {
    /// Running app processes started from this executable, excluding this
    /// command. A stale pre-`ts` indexer counts as one: it holds the index
    /// lock the restarted service needs.
    fn instances(&mut self) -> Vec<u32>;
    /// This executable.
    fn executable(&self) -> PathBuf;
    fn terminate(&mut self, pid: u32) -> Result<(), String>;
    /// Restarts the shared indexer service through `ts restart`.
    fn restart_indexer(&mut self) -> Result<ts_client::ServiceReport, String>;
    /// Starts this executable's app detached.
    fn spawn_app(&mut self) -> Result<(), String>;
}

/// Restarts the app and the indexer of `host`'s executable. Returns what
/// it did; a failure anywhere still runs the rest, and all failures join
/// the error.
#[cfg(any(target_os = "linux", test))]
pub fn restart(host: &mut impl Host) -> Result<Vec<String>, String> {
    let instances = host.instances();
    let mut actions = Vec::new();
    let mut failures = Vec::new();

    for pid in instances {
        match host.terminate(pid) {
            Ok(()) => actions.push(format!("stopped Toka (pid {pid})")),
            Err(error) => failures.push(error),
        }
    }

    match host.restart_indexer() {
        Ok(report) => {
            actions.extend(report.actions.into_iter().map(|action| action.message));
            if let Some(error) = report.error {
                failures.push(error);
            }
        }
        Err(error) => failures.push(error),
    }

    match host.spawn_app() {
        Ok(()) => actions.push("started Toka".to_owned()),
        Err(error) => failures.push(error),
    }

    if failures.is_empty() {
        Ok(actions)
    } else {
        Err(failures.join("\n"))
    }
}

/// The `restart` subcommand entry point.
pub fn run(
    arguments: &[OsString],
    run_ts: impl FnMut(&[OsString]) -> Result<TsOutput, String>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> i32 {
    if arguments.len() == 1 && (arguments[0] == "--help" || arguments[0] == "-h") {
        let _ = writeln!(stdout, "{HELP}");
        return 0;
    }
    if !arguments.is_empty() {
        let _ = writeln!(stderr, "Restart does not take arguments.\n{USAGE}");
        return 2;
    }
    #[cfg(target_os = "linux")]
    {
        let mut host = match SystemHost::new(run_ts) {
            Ok(host) => host,
            Err(error) => {
                let _ = writeln!(stderr, "toka restart: {error}");
                return 1;
            }
        };
        match restart(&mut host) {
            Ok(actions) => {
                for action in actions {
                    let _ = writeln!(stdout, "{action}");
                }
                0
            }
            Err(error) => {
                let _ = writeln!(stderr, "toka restart: {error}");
                1
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = run_ts;
        let _ = stdout;
        let _ = writeln!(stderr, "toka restart: only supported on Linux");
        1
    }
}

pub const USAGE: &str = "Usage: toka restart";
pub const HELP: &str = "Usage: toka restart\n\nRestart this Toka's app and its indexer. Only the app that was called is restarted: a worktree build leaves the installed app alone, and the installed app leaves worktree builds alone. The indexer is the shared `ts` service, restarted with `ts restart`.";

#[cfg(target_os = "linux")]
struct SystemHost<F> {
    executable: PathBuf,
    run_ts: F,
}

#[cfg(target_os = "linux")]
impl<F> SystemHost<F> {
    fn new(run_ts: F) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .map_err(|error| format!("could not find the toka executable: {error}"))?;
        Ok(Self { executable, run_ts })
    }
}

/// The executable behind `/proc/<pid>/exe`, without the ` (deleted)` suffix the
/// kernel adds after an update replaced the file.
#[cfg(target_os = "linux")]
fn process_executable(proc_root: &Path, pid: u32) -> Option<PathBuf> {
    let target = fs::read_link(proc_root.join(pid.to_string()).join("exe")).ok()?;
    let text = target.to_string_lossy();
    Some(PathBuf::from(
        text.strip_suffix(" (deleted)").unwrap_or(&text).to_owned(),
    ))
}

/// The pids of `executable` under `proc_root`, other than `own_pid` and
/// other CLI commands (such as a running `toka search`). A stale
/// pre-`ts` indexer is just another process of this executable — and one
/// that has to go, since it holds the index lock.
#[cfg(target_os = "linux")]
pub fn find_instances(proc_root: &Path, executable: &Path, own_pid: u32) -> Vec<u32> {
    let Ok(entries) = fs::read_dir(proc_root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        if pid == own_pid {
            continue;
        }
        let Some(process) = process_executable(proc_root, pid) else {
            continue;
        };
        if !same_executable(&process, executable) {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let arguments: Vec<std::ffi::OsString> = cmdline
            .split(|byte| *byte == 0)
            .map(|part| {
                use std::os::unix::ffi::OsStringExt;
                std::ffi::OsString::from_vec(part.to_vec())
            })
            .collect();
        if crate::cli::is_command(&arguments) {
            continue;
        }
        found.push(pid);
    }
    found.sort();
    found
}

#[cfg(target_os = "linux")]
impl<F> Host for SystemHost<F>
where
    F: FnMut(&[OsString]) -> Result<TsOutput, String>,
{
    fn instances(&mut self) -> Vec<u32> {
        find_instances(Path::new("/proc"), &self.executable, std::process::id())
    }

    fn executable(&self) -> PathBuf {
        self.executable.clone()
    }

    fn terminate(&mut self, pid: u32) -> Result<(), String> {
        let pid_t = pid as libc::pid_t;
        // SAFETY: plain signal delivery to a process id we just discovered.
        unsafe { libc::kill(pid_t, libc::SIGTERM) };
        for _ in 0..50 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
        // SAFETY: as above.
        unsafe { libc::kill(pid_t, libc::SIGKILL) };
        thread::sleep(Duration::from_millis(200));
        if Path::new(&format!("/proc/{pid}")).exists() {
            Err(format!("could not stop process {pid}"))
        } else {
            Ok(())
        }
    }

    fn restart_indexer(&mut self) -> Result<ts_client::ServiceReport, String> {
        let args = [OsString::from("restart"), OsString::from("--json")];
        let output = (self.run_ts)(&args)?;
        ts_client::service_report_from_output("restart", &output)
    }

    fn spawn_app(&mut self) -> Result<(), String> {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(&self.executable);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // SAFETY: setsid is async-signal-safe; it detaches the child so it
        // survives this command and its terminal.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        command
            .spawn()
            .map(|_| ())
            .map_err(|error| format!("could not start {}: {error}", self.executable.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        executable: PathBuf,
        instances: Vec<u32>,
        indexer: Result<ts_client::ServiceReport, String>,
        calls: Vec<String>,
    }

    impl Fake {
        fn new(instances: Vec<u32>) -> Self {
            Self {
                executable: "/installed/toka".into(),
                instances,
                indexer: Ok(ts_client::ServiceReport {
                    actions: vec![ts_client::ServiceAction {
                        message: "restarted the indexer".to_owned(),
                        changed: true,
                    }],
                    error: None,
                }),
                calls: Vec::new(),
            }
        }
    }

    impl Host for Fake {
        fn instances(&mut self) -> Vec<u32> {
            self.instances.clone()
        }
        fn executable(&self) -> PathBuf {
            self.executable.clone()
        }
        fn terminate(&mut self, pid: u32) -> Result<(), String> {
            self.calls.push(format!("terminate {pid}"));
            Ok(())
        }
        fn restart_indexer(&mut self) -> Result<ts_client::ServiceReport, String> {
            self.calls.push("restart-indexer".into());
            self.indexer.clone()
        }
        fn spawn_app(&mut self) -> Result<(), String> {
            self.calls.push("spawn-app".into());
            Ok(())
        }
    }

    #[test]
    fn restart_stops_the_app_restarts_the_indexer_and_starts_again() {
        let mut host = Fake::new(vec![10, 11]);

        let actions = restart(&mut host).unwrap();

        assert_eq!(
            host.calls,
            [
                "terminate 10",
                "terminate 11",
                "restart-indexer",
                "spawn-app"
            ]
        );
        assert_eq!(
            actions,
            [
                "stopped Toka (pid 10)",
                "stopped Toka (pid 11)",
                "restarted the indexer",
                "started Toka",
            ]
        );
    }

    #[test]
    fn it_restarts_the_indexer_even_when_nothing_was_running() {
        let mut host = Fake::new(vec![]);

        restart(&mut host).unwrap();

        assert_eq!(host.calls, ["restart-indexer", "spawn-app"]);
    }

    #[test]
    fn a_failed_indexer_restart_is_reported_but_the_app_still_restarts() {
        let mut host = Fake::new(vec![40]);
        host.indexer = Ok(ts_client::ServiceReport {
            actions: vec![ts_client::ServiceAction {
                message: "stopped the indexer".to_owned(),
                changed: true,
            }],
            error: Some("could not start it again".to_owned()),
        });

        let error = restart(&mut host).unwrap_err();

        // The partial actions ran; only the error joins the failure.
        assert_eq!(error, "could not start it again");
        assert_eq!(host.calls, ["terminate 40", "restart-indexer", "spawn-app"]);
    }

    #[test]
    fn a_failed_stop_is_reported_but_the_rest_still_runs() {
        struct Stubborn(Fake);
        impl Host for Stubborn {
            fn instances(&mut self) -> Vec<u32> {
                self.0.instances()
            }
            fn executable(&self) -> PathBuf {
                self.0.executable()
            }
            fn terminate(&mut self, pid: u32) -> Result<(), String> {
                Err(format!("could not stop process {pid}"))
            }
            fn restart_indexer(&mut self) -> Result<ts_client::ServiceReport, String> {
                self.0.restart_indexer()
            }
            fn spawn_app(&mut self) -> Result<(), String> {
                self.0.spawn_app()
            }
        }
        let mut host = Stubborn(Fake::new(vec![40]));

        let error = restart(&mut host).unwrap_err();

        assert_eq!(error, "could not stop process 40");
        assert_eq!(host.0.calls, ["restart-indexer", "spawn-app"]);
    }

    #[test]
    fn restart_reports_ts_spawn_failures() {
        let mut host = Fake::new(vec![]);
        host.indexer = Err("Toka could not find the `ts` search helper.".to_owned());

        let error = restart(&mut host).unwrap_err();

        assert_eq!(error, "Toka could not find the `ts` search helper.");
        assert_eq!(host.calls, ["restart-indexer", "spawn-app"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_processes_of_the_given_executable_including_stale_indexers() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let mine = root.path().join("mine");
        let other = root.path().join("other");
        fs::write(&mine, b"").unwrap();
        fs::write(&other, b"").unwrap();
        let add = |pid: u32, exe: &Path, cmdline: &[u8]| {
            let dir = root.path().join(pid.to_string());
            fs::create_dir(&dir).unwrap();
            symlink(exe, dir.join("exe")).unwrap();
            fs::write(dir.join("cmdline"), cmdline).unwrap();
        };
        add(1, &mine, b"toka\0");
        // A stale pre-`ts` indexer: same executable, not a CLI command,
        // so it is found and stopped like any app process.
        add(2, &mine, b"toka\0--indexer\0");
        add(3, &other, b"toka\0");
        add(4, &mine, b"toka\0search\0cats\0");
        add(5, &mine, b"toka\0");

        let found = find_instances(root.path(), &mine, 5);

        assert_eq!(found, vec![1, 2]);
    }
}
