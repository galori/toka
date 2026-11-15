//! `toka restart`: restart this Toka build's app and indexer, and nothing else.
//!
//! A worktree build and the installed build can run side by side. Instances
//! are told apart by the executable they were started from, so the command
//! only ever touches processes (and the systemd indexer service) that belong
//! to the executable that was called.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
use std::{fs, process::Command, thread, time::Duration};

/// One running process of the executable being restarted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub pid: u32,
    /// The background `--indexer` process, as opposed to the app window.
    pub is_indexer: bool,
}

/// Everything `restart` needs from the machine, so the decisions can be tested.
pub trait Host {
    /// Running processes started from this executable, excluding this command.
    fn instances(&mut self) -> Vec<Instance>;
    /// The executable the `toka-indexer.service` user unit runs, if it has one.
    fn service_executable(&mut self) -> Option<PathBuf>;
    /// This executable.
    fn executable(&self) -> PathBuf;
    fn terminate(&mut self, pid: u32) -> Result<(), String>;
    fn restart_service(&mut self) -> Result<(), String>;
    /// Starts this executable detached, with `--indexer` or no argument.
    fn spawn(&mut self, indexer: bool) -> Result<(), String>;
}

/// Restarts the app and the indexer of `host`'s executable. Returns what it did.
pub fn restart(host: &mut impl Host) -> Result<Vec<String>, String> {
    let executable = host.executable();
    let service_is_ours = host
        .service_executable()
        .is_some_and(|service| same_executable(&service, &executable));
    let instances = host.instances();
    let mut actions = Vec::new();
    let mut failures = Vec::new();
    let mut indexer_was_running = false;

    for instance in &instances {
        if instance.is_indexer {
            indexer_was_running = true;
            // The service restart below replaces a service-managed indexer.
            if service_is_ours {
                continue;
            }
        }
        match host.terminate(instance.pid) {
            Ok(()) => actions.push(format!(
                "stopped {} (pid {})",
                if instance.is_indexer {
                    "indexer"
                } else {
                    "Toka"
                },
                instance.pid
            )),
            Err(error) => failures.push(error),
        }
    }

    if service_is_ours {
        match host.restart_service() {
            Ok(()) => actions.push("restarted toka-indexer.service".to_owned()),
            Err(error) => failures.push(error),
        }
    } else if indexer_was_running {
        match host.spawn(true) {
            Ok(()) => actions.push("started indexer".to_owned()),
            Err(error) => failures.push(error),
        }
    }

    match host.spawn(false) {
        Ok(()) => actions.push("started Toka".to_owned()),
        Err(error) => failures.push(error),
    }

    if failures.is_empty() {
        Ok(actions)
    } else {
        Err(failures.join("\n"))
    }
}

fn same_executable(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Pulls the program path out of `systemctl show -p ExecStart` output, which
/// looks like `ExecStart={ path=/bin/toka ; argv[]=/bin/toka --indexer ; ... }`.
pub fn parse_service_executable(show_output: &str) -> Option<PathBuf> {
    let start = show_output.find("path=")? + "path=".len();
    let rest = &show_output[start..];
    let end = rest.find(" ;").unwrap_or(rest.len());
    let path = rest[..end].trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// The `restart` subcommand entry point.
pub fn run(
    arguments: &[std::ffi::OsString],
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
        let mut host = match SystemHost::new() {
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
        let _ = stdout;
        let _ = writeln!(stderr, "toka restart: only supported on Linux");
        1
    }
}

pub const USAGE: &str = "Usage: toka restart";
pub const HELP: &str = "Usage: toka restart\n\nRestart this Toka's app and its indexer. Only the Toka that was called is restarted: a worktree build leaves the installed Toka alone, and the installed Toka leaves worktree builds alone.";

#[cfg(target_os = "linux")]
struct SystemHost {
    executable: PathBuf,
}

#[cfg(target_os = "linux")]
impl SystemHost {
    fn new() -> Result<Self, String> {
        let executable = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .map_err(|error| format!("could not find the toka executable: {error}"))?;
        Ok(Self { executable })
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

/// Finds processes of `executable` under `proc_root`, other than `own_pid` and
/// other CLI commands (such as a running `toka search`).
#[cfg(target_os = "linux")]
pub fn find_instances(proc_root: &Path, executable: &Path, own_pid: u32) -> Vec<Instance> {
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
        let is_indexer = arguments
            .get(1)
            .is_some_and(|argument| argument == "--indexer");
        if !is_indexer && crate::cli::is_command(&arguments) {
            continue;
        }
        found.push(Instance { pid, is_indexer });
    }
    found.sort_by_key(|instance| instance.pid);
    found
}

#[cfg(target_os = "linux")]
impl Host for SystemHost {
    fn instances(&mut self) -> Vec<Instance> {
        find_instances(Path::new("/proc"), &self.executable, std::process::id())
    }

    fn service_executable(&mut self) -> Option<PathBuf> {
        let output = Command::new("systemctl")
            .args(["--user", "show", "-p", "ExecStart", "toka-indexer.service"])
            .output()
            .ok()?;
        parse_service_executable(&String::from_utf8_lossy(&output.stdout))
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

    fn restart_service(&mut self) -> Result<(), String> {
        let status = Command::new("systemctl")
            .args(["--user", "restart", "toka-indexer.service"])
            .status()
            .map_err(|error| format!("could not restart the indexer service: {error}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "the indexer service could not be restarted (systemctl exited with {status})"
            ))
        }
    }

    fn spawn(&mut self, indexer: bool) -> Result<(), String> {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(&self.executable);
        if indexer {
            command.arg("--indexer");
        }
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
        service: Option<PathBuf>,
        instances: Vec<Instance>,
        calls: Vec<String>,
    }

    impl Fake {
        fn new(service: Option<&str>, instances: Vec<Instance>) -> Self {
            Self {
                executable: "/installed/toka".into(),
                service: service.map(PathBuf::from),
                instances,
                calls: Vec::new(),
            }
        }
    }

    impl Host for Fake {
        fn instances(&mut self) -> Vec<Instance> {
            self.instances.clone()
        }
        fn service_executable(&mut self) -> Option<PathBuf> {
            self.service.clone()
        }
        fn executable(&self) -> PathBuf {
            self.executable.clone()
        }
        fn terminate(&mut self, pid: u32) -> Result<(), String> {
            self.calls.push(format!("terminate {pid}"));
            Ok(())
        }
        fn restart_service(&mut self) -> Result<(), String> {
            self.calls.push("restart-service".into());
            Ok(())
        }
        fn spawn(&mut self, indexer: bool) -> Result<(), String> {
            self.calls.push(
                if indexer {
                    "spawn-indexer"
                } else {
                    "spawn-app"
                }
                .into(),
            );
            Ok(())
        }
    }

    fn app(pid: u32) -> Instance {
        Instance {
            pid,
            is_indexer: false,
        }
    }
    fn indexer(pid: u32) -> Instance {
        Instance {
            pid,
            is_indexer: true,
        }
    }

    #[test]
    fn the_installed_toka_restarts_its_app_and_its_service() {
        let mut host = Fake::new(Some("/installed/toka"), vec![app(10), indexer(11)]);

        restart(&mut host).unwrap();

        // The service-managed indexer is replaced by the service restart, not killed.
        assert_eq!(host.calls, ["terminate 10", "restart-service", "spawn-app"]);
    }

    #[test]
    fn a_worktree_toka_never_touches_the_installed_service() {
        let mut host = Fake::new(Some("/other/toka"), vec![app(20), indexer(21)]);

        restart(&mut host).unwrap();

        assert!(!host.calls.contains(&"restart-service".to_owned()));
        assert_eq!(
            host.calls,
            ["terminate 20", "terminate 21", "spawn-indexer", "spawn-app"]
        );
    }

    #[test]
    fn a_worktree_without_a_running_indexer_does_not_start_one() {
        let mut host = Fake::new(None, vec![app(30)]);

        restart(&mut host).unwrap();

        assert_eq!(host.calls, ["terminate 30", "spawn-app"]);
    }

    #[test]
    fn it_starts_toka_even_when_nothing_was_running() {
        let mut host = Fake::new(Some("/installed/toka"), vec![]);

        restart(&mut host).unwrap();

        assert_eq!(host.calls, ["restart-service", "spawn-app"]);
    }

    #[test]
    fn a_failed_stop_is_reported_but_the_rest_still_runs() {
        struct Stubborn(Fake);
        impl Host for Stubborn {
            fn instances(&mut self) -> Vec<Instance> {
                self.0.instances()
            }
            fn service_executable(&mut self) -> Option<PathBuf> {
                self.0.service_executable()
            }
            fn executable(&self) -> PathBuf {
                self.0.executable()
            }
            fn terminate(&mut self, pid: u32) -> Result<(), String> {
                Err(format!("could not stop process {pid}"))
            }
            fn restart_service(&mut self) -> Result<(), String> {
                self.0.restart_service()
            }
            fn spawn(&mut self, indexer: bool) -> Result<(), String> {
                self.0.spawn(indexer)
            }
        }
        let mut host = Stubborn(Fake::new(Some("/installed/toka"), vec![app(40)]));

        let error = restart(&mut host).unwrap_err();

        assert_eq!(error, "could not stop process 40");
        assert_eq!(host.0.calls, ["restart-service", "spawn-app"]);
    }

    #[test]
    fn parses_the_program_path_from_systemctl_show() {
        let output = "ExecStart={ path=/home/me/.local/bin/toka ; argv[]=/home/me/.local/bin/toka --indexer ; ignore_errors=no }\n";
        assert_eq!(
            parse_service_executable(output),
            Some(PathBuf::from("/home/me/.local/bin/toka"))
        );
        assert_eq!(parse_service_executable("ExecStart=\n"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_only_processes_of_the_given_executable() {
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
        add(2, &mine, b"toka\0--indexer\0");
        add(3, &other, b"toka\0");
        add(4, &mine, b"toka\0search\0cats\0");
        add(5, &mine, b"toka\0");

        let found = find_instances(root.path(), &mine, 5);

        assert_eq!(found, vec![app(1), indexer(2)]);
    }
}
