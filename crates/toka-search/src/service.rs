//! `ts` service management: running the indexer daemon as a systemd user
//! unit (Linux) or a launchd LaunchAgent (macOS).
//!
//! `ts install` writes the service file pointing at `ts`'s own executable,
//! enables it, and starts it; on Linux it also migrates the old
//! `toka-indexer.service` away. Every command is idempotent and reports
//! what it did versus the state it already found, so re-running any of
//! them converges instead of failing.
//!
//! Everything a command needs from the machine hangs off [`Host`], so the
//! decisions are unit-testable with a fake; [`SystemHost`] is the real one.
//! The platform dispatch goes through an explicit [`ServiceManager`], so the
//! systemd and launchd flows are both testable on either OS.

use serde::Serialize;
use std::path::{Path, PathBuf};

/// Which service manager owns the daemon on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceManager {
    Systemd,
    Launchd,
}

impl ServiceManager {
    /// The manager for this platform, or `None` where `ts` manages no
    /// service (today: everywhere but Linux and macOS).
    pub fn current() -> Option<Self> {
        #[cfg(target_os = "linux")]
        return Some(Self::Systemd);
        #[cfg(target_os = "macos")]
        return Some(Self::Launchd);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        return None;
    }
}

/// What `ts status` reports about the service: whether it is installed and,
/// once installed, whether the manager has it enabled and running. Every
/// field is `None` where it cannot be known: `installed` where there is no
/// home to look in (or no manager on this platform), the other two where
/// the manager cannot say (or its answer was unreadable) — and always
/// where nothing is installed, since a hand-started daemon may still run
/// there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceState {
    pub installed: Option<bool>,
    pub enabled: Option<bool>,
    pub running: Option<bool>,
}

impl ServiceState {
    pub fn unknown() -> Self {
        Self {
            installed: None,
            enabled: None,
            running: None,
        }
    }
}

/// One thing a service command did — or found already done. Human output
/// prints the message; `--json` carries the pair, so the app can tell what
/// changed without parsing prose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Action {
    pub message: String,
    pub changed: bool,
}

/// Everything one service command did, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub actions: Vec<Action>,
}

impl Outcome {
    fn did(&mut self, message: impl Into<String>) {
        self.actions.push(Action {
            message: message.into(),
            changed: true,
        });
    }

    fn already(&mut self, message: impl Into<String>) {
        self.actions.push(Action {
            message: message.into(),
            changed: false,
        });
    }
}

/// The five service commands, as one value the CLI hands to [`perform`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceOp {
    Install { link: bool },
    Uninstall { purge_data: bool },
    Restart,
    Start,
    Stop,
}

impl ServiceOp {
    /// The command word, shared by human errors and the `--json` document.
    pub fn name(self) -> &'static str {
        match self {
            Self::Install { .. } => "install",
            Self::Uninstall { .. } => "uninstall",
            Self::Restart => "restart",
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }
}

/// A finished manager invocation: its exit code (`None` on signal) and
/// what it printed. A nonzero exit is data, not an error — only a spawn
/// failure errors, since only then did nothing run at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandResult {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Everything service management needs from the machine, so the decisions
/// can be tested without touching real services.
pub trait Host {
    fn home(&self) -> PathBuf;
    /// The user config directory: `XDG_CONFIG_HOME` when set, otherwise
    /// `~/.config`. The systemd unit lives under it; the legacy unit
    /// stays under `~/.config`, where the old installer always wrote it.
    fn config_dir(&self) -> PathBuf;
    /// The `ts` executable the service files point at.
    fn executable(&self) -> PathBuf;
    /// The temp directory: an executable under it is an ephemeral copy —
    /// an AppImage mount, a deleted-while-running binary staged in tmp —
    /// that `install` refuses to register as the service executable.
    fn temp_dir(&self) -> PathBuf;
    /// The login user id, for `launchctl`'s `gui/<uid>` domain.
    fn uid(&self) -> u32;
    /// The login name, for `loginctl enable-linger`.
    fn user(&self) -> Option<String>;
    fn run(&mut self, program: &str, args: &[&str]) -> Result<CommandResult, String>;
    /// The index data an uninstall purges: the index's known files, the
    /// logs, the thumbnail cache. Never media. A host method (not a free
    /// function) so tests point it at scratch instead of the real index.
    fn purge_paths(&mut self) -> Result<Vec<crate::reset::ResetTarget>, String>;
    /// The environment assignments the service file pins, so the daemon
    /// resolves the same locations the installing CLI used.
    fn service_env(&self) -> Vec<(String, String)>;
    /// Whether another supervisor holds the daemon lock right now, so a
    /// purge or reset under a live daemon refuses instead of deleting.
    fn daemon_lock_held(&mut self) -> Result<bool, String>;
}

/// The real machine.
pub struct SystemHost {
    home: PathBuf,
    config_dir: PathBuf,
    executable: PathBuf,
    uid: u32,
    user: Option<String>,
}

impl SystemHost {
    pub fn new() -> Result<Self, String> {
        let mut host = Self::for_query()?;
        host.executable = std::env::current_exe()
            .map_err(|error| format!("could not find the ts executable: {error}"))?;
        Ok(host)
    }

    /// A host for read-only probes (`ts status`): resolves only the home
    /// directory, never the executable, so an unresolvable current
    /// executable cannot flip an installed service to unknown.
    pub fn for_query() -> Result<Self, String> {
        let home = dirs::home_dir()
            .ok_or_else(|| "Toka could not find your home directory.".to_owned())?;
        let config_dir = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        #[cfg(unix)]
        // SAFETY: `getuid` takes no arguments and cannot fail.
        let uid = unsafe { libc::getuid() };
        #[cfg(not(unix))]
        let uid = 0;
        let user = std::env::var("USER").ok().filter(|user| !user.is_empty());
        Ok(Self {
            home,
            config_dir,
            executable: PathBuf::new(),
            uid,
            user,
        })
    }
}

impl Host for SystemHost {
    fn home(&self) -> PathBuf {
        self.home.clone()
    }

    fn config_dir(&self) -> PathBuf {
        self.config_dir.clone()
    }

    fn executable(&self) -> PathBuf {
        self.executable.clone()
    }

    fn temp_dir(&self) -> PathBuf {
        std::env::temp_dir()
    }

    fn uid(&self) -> u32 {
        self.uid
    }

    fn user(&self) -> Option<String> {
        self.user.clone()
    }

    fn run(&mut self, program: &str, args: &[&str]) -> Result<CommandResult, String> {
        let output = std::process::Command::new(program)
            .args(args)
            .output()
            .map_err(|error| format!("could not run {program}: {error}"))?;
        Ok(CommandResult {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn purge_paths(&mut self) -> Result<Vec<crate::reset::ResetTarget>, String> {
        reset_data_paths()
    }

    fn service_env(&self) -> Vec<(String, String)> {
        let data_home = std::env::var("XDG_DATA_HOME").ok();
        let config_home = std::env::var("XDG_CONFIG_HOME").ok();
        pinned_service_env(&self.home, data_home.as_deref(), config_home.as_deref())
    }

    fn daemon_lock_held(&mut self) -> Result<bool, String> {
        crate::indexer_daemon::daemon_lock_held(&crate::owned_index::database_path()?)
    }
}

/// The search state a reset removes: the index's known files (the
/// database, its sidecars, the rename overlay, the legacy daemon lock),
/// every log file, and the thumbnail cache — both the current uid-scoped
/// root and the pre-upgrade unscoped one uid-scoping orphaned, so
/// upgrade+reset leaves no stale cache behind. Never media, tags, or
/// add-ons — the same set the app's reset removes, minus the app's own
/// caches. Shared by `uninstall --purge-data` and `ts reset`, so the two
/// can never drift apart. The index directory itself is never removed, so
/// foreign content stored there always survives; the index artifacts are
/// file-only targets, so a directory at one of their paths refuses
/// instead of deleting.
pub fn reset_data_paths() -> Result<Vec<crate::reset::ResetTarget>, String> {
    use crate::reset::ResetTarget;
    let database = crate::owned_index::database_path()?;
    let mut paths: Vec<ResetTarget> = crate::owned_index::index_data_files(&database)
        .into_iter()
        .map(ResetTarget::file)
        .collect();
    paths.extend(crate::search_log::reset_paths());
    // The daemon's own log: on macOS it sits outside the search-log folder
    // removed wholesale above, so it needs its own file-only target —
    // file-only, so unrelated files beside it survive. Where the wholesale
    // folder already covers it the plan drops this as nested, a no-op.
    if let Ok(log) = crate::indexer_daemon::indexer_log_path() {
        paths.push(ResetTarget::file(log));
    }
    paths.push(ResetTarget::dir(crate::thumbnails::cache_output_root()));
    paths.push(ResetTarget::dir(
        crate::thumbnails::legacy_cache_output_root(),
    ));
    Ok(paths)
}

/// The XDG assignments a service file pins: the EFFECTIVE base
/// directories the installing shell resolves, defaults included. A daemon
/// inherits its manager's environment, not the installing shell's, so
/// pinning only non-default values splits the brain whenever the two
/// sides disagree about the default: a manager with
/// `XDG_DATA_HOME=/srv/toka-data` and a shell that leaves it unset (or
/// sets it explicitly to `~/.local/share`) would otherwise open two
/// different indexes. An unset, relative, or empty value resolves to the
/// default — `dirs` ignores the last two — so the default pins; only a
/// non-empty absolute value pins itself.
fn pinned_service_env(
    home: &Path,
    xdg_data_home: Option<&str>,
    xdg_config_home: Option<&str>,
) -> Vec<(String, String)> {
    [
        ("XDG_DATA_HOME", ".local/share", xdg_data_home),
        ("XDG_CONFIG_HOME", ".config", xdg_config_home),
    ]
    .into_iter()
    .map(|(name, default, value)| {
        let effective = value
            .filter(|value| !value.is_empty() && Path::new(value).is_absolute())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| home.join(default).to_string_lossy().into_owned());
        (name.to_owned(), effective)
    })
    .collect()
}

/// The unit file `ts install` writes, under the user config directory
/// (`XDG_CONFIG_HOME` when set, otherwise `~/.config`). A unit under any
/// other config dir — installed before `ts` honored `XDG_CONFIG_HOME`, or
/// orphaned by an `XDG_CONFIG_HOME` change mid-session — is neither read
/// nor removed: the user manager loads only the current dir's units, so
/// the stale file is inert litter, not a second service.
pub fn unit_path(config_dir: &Path) -> PathBuf {
    config_dir
        .join("systemd")
        .join("user")
        .join(crate::config::TS_SERVICE_NAME)
}

/// The old unit file, written by the app installer and migrated away by
/// `ts install`.
pub(crate) fn legacy_unit_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join("systemd")
        .join("user")
        .join(crate::config::INDEXER_SERVICE_NAME)
}

/// The LaunchAgent plist `ts install` writes.
pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", crate::config::TS_LAUNCHD_LABEL))
}

/// Where `ts install --link` links the executable.
fn link_path(home: &Path) -> PathBuf {
    home.join(".local").join("bin").join("ts")
}

/// The systemd user unit, ported from `scripts/install-linux.mjs`: same
/// restart, niceness, and idle I/O scheduling, now running
/// `ts daemon run` instead of `toka --indexer`. `Type=exec` waits for the
/// daemon's execve before reporting started, so a missing binary fails
/// the start instead of silently running nothing. `env` pins the XDG
/// locations the installing shell resolves (see [`pinned_service_env`]),
/// one `Environment=` line each, so the daemon resolves the same index
/// the CLI used. Fails when the executable path — or a pinned value —
/// cannot survive its line.
pub fn render_unit(executable: &Path, env: &[(String, String)]) -> Result<String, String> {
    let mut environment = String::new();
    for (name, value) in env {
        environment.push_str(&format!(
            "Environment=\"{name}={}\"\n",
            systemd_escape(value, &format!("the {name} value"))?,
        ));
    }
    Ok(format!(
        "[Unit]\nDescription=Toka search indexer\n\n[Service]\nType=exec\nExecStart=\"{}\" daemon run\n{environment}Restart=on-failure\nRestartSec=2\nNice=10\nIOSchedulingClass=idle\n\n[Install]\nWantedBy=default.target\n",
        systemd_escape(&executable.to_string_lossy(), "the ts path")?,
    ))
}

/// Quotes one value for a systemd unit line: backslashes and double
/// quotes escaped, wrapped in double quotes by the caller — plus the two
/// cases the app installer never handled. A `%` would expand as a systemd
/// specifier (`%h`, `%u`, …), so it doubles; a newline would break out of
/// the directive entirely, so it errors instead of writing a corrupt unit.
fn systemd_escape(text: &str, what: &str) -> Result<String, String> {
    if text.contains('\n') || text.contains('\r') {
        return Err(format!(
            "{what} cannot be installed as a service: it contains a newline: {text}"
        ));
    }
    Ok(text
        .replace('%', "%%")
        .replace('\\', "\\\\")
        .replace('"', "\\\""))
}

/// The launchd LaunchAgent plist: kept alive in the background with idle
/// I/O priority, started at login, running `ts daemon run` with no shell
/// in between. The explicit `PATH` covers both Homebrew prefixes plus the
/// system directories: agents inherit launchd's minimal environment, not
/// the installing shell's, so without it a Homebrew ffmpeg would be
/// invisible and background thumbnail generation would silently fail. A
/// fixed `PATH` stays correct when Homebrew is installed later, which
/// resolving absolute tool paths at install time would not. `env` pins
/// the XDG locations the installing shell resolves (see
/// [`pinned_service_env`]) beside that `PATH`, so the agent resolves
/// the same index the CLI used.
pub fn render_plist(executable: &Path, env: &[(String, String)]) -> String {
    let mut environment = String::new();
    for (name, value) in env {
        environment.push_str(&format!(
            "\n\t\t<key>{name}</key>\n\t\t<string>{}</string>",
            xml_escape(value),
        ));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>{}</string>\n\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>\n\t\t<string>daemon</string>\n\t\t<string>run</string>\n\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n\t<key>ProcessType</key>\n\t<string>Background</string>\n\t<key>LowPriorityIO</key>\n\t<true/>\n\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>PATH</key>\n\t\t<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>{environment}\n\t</dict>\n</dict>\n</plist>\n",
        crate::config::TS_LAUNCHD_LABEL,
        xml_escape(&executable.to_string_lossy()),
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A path for human output: under the home directory it reads with `~`.
fn display_path(home: &Path, path: &Path) -> String {
    if let Ok(rest) = path.strip_prefix(home) {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Reads the service state through this platform's manager: what
/// `ts status` reports. Never writes anything.
pub fn query(host: &mut impl Host) -> ServiceState {
    match ServiceManager::current() {
        Some(manager) => query_on(host, manager),
        None => ServiceState::unknown(),
    }
}

/// [`query`] against an explicit manager, so both flows are testable on
/// either OS.
pub fn query_on(host: &mut impl Host, manager: ServiceManager) -> ServiceState {
    match manager {
        ServiceManager::Systemd => query_systemd(host),
        ServiceManager::Launchd => query_launchd(host),
    }
}

/// Runs one service command to convergence, reporting what it did versus
/// the state it already found. A mid-command failure still reports what it
/// completed before failing, so a failed uninstall names the steps that
/// already ran instead of discarding them.
pub fn perform(host: &mut impl Host, op: ServiceOp) -> Result<Outcome, (String, Outcome)> {
    match ServiceManager::current() {
        Some(manager) => perform_on(host, manager, op),
        None => Err((
            "service management is only supported on Linux and macOS.".to_owned(),
            Outcome::default(),
        )),
    }
}

/// [`perform`] against an explicit manager, so both flows are testable on
/// either OS.
pub fn perform_on(
    host: &mut impl Host,
    manager: ServiceManager,
    op: ServiceOp,
) -> Result<Outcome, (String, Outcome)> {
    match manager {
        ServiceManager::Systemd => match op {
            ServiceOp::Install { link } => install_systemd(host, link),
            ServiceOp::Uninstall { purge_data } => uninstall_systemd(host, purge_data),
            ServiceOp::Restart => restart_systemd(host),
            ServiceOp::Start => start_systemd(host),
            ServiceOp::Stop => stop_systemd(host),
        },
        ServiceManager::Launchd => match op {
            ServiceOp::Install { link } => install_launchd(host, link),
            ServiceOp::Uninstall { purge_data } => uninstall_launchd(host, purge_data),
            ServiceOp::Restart => restart_launchd(host),
            ServiceOp::Start => start_launchd(host),
            ServiceOp::Stop => stop_launchd(host),
        },
    }
}

/// Attaches the actions completed so far to a mid-command failure, so the
/// partial report survives the error instead of being discarded.
fn with_outcome<T>(
    result: Result<T, String>,
    outcome: &mut Outcome,
) -> Result<T, (String, Outcome)> {
    result.map_err(|error| (error, std::mem::take(outcome)))
}

/// Stops every indexer service that is running — the `ts` one and, while
/// the migration is still rolling out, the old `toka --indexer` one — so a
/// reset cannot delete state out from under a live supervisor. A service
/// that is not running needs no stopping; a running service that refuses
/// to stop fails, and then the reset must not delete anything (see
/// [`crate::reset::apply`]). An unreadable manager answer fails closed
/// when a service file is still installed — a lingering service, or a bus
/// this environment cannot reach, may still run there — and only proceeds
/// when nothing is installed. After the stops, a held daemon lock fails
/// too: a hand-started daemon survives every stop above.
pub fn stop_indexer_services() -> Result<(), String> {
    // A query host suffices: stopping never points a service file at the
    // executable, so an unresolvable current executable must not fail the
    // reset's pre-stop.
    let mut host = SystemHost::for_query()?;
    let Some(manager) = ServiceManager::current() else {
        return Ok(());
    };
    stop_for_reset(&mut host, manager)
}

/// [`stop_indexer_services`] against an explicit manager, for tests.
fn stop_for_reset(host: &mut impl Host, manager: ServiceManager) -> Result<(), String> {
    match manager {
        ServiceManager::Systemd => {
            let home = host.home();
            let config = host.config_dir();
            for (unit, file) in [
                (crate::config::TS_SERVICE_NAME, unit_path(&config)),
                (crate::config::INDEXER_SERVICE_NAME, legacy_unit_path(&home)),
            ] {
                // One read for both the probe and the guard: a delete or
                // create between two reads would skew the branch taken
                // against the state probed.
                let present = file.is_file();
                match systemd_liveness(host, unit, present) {
                    Liveness::Active | Liveness::Deactivating => {
                        systemd_command(host, &["stop", unit])?;
                    }
                    Liveness::Stopped => {}
                    Liveness::Unknown if present => {
                        return Err(format!(
                            "could not ask systemd about {unit}, and it is still installed: \
                             refusing to reset while the indexer may be running."
                        ));
                    }
                    Liveness::Unknown => {}
                }
            }
        }
        ServiceManager::Launchd => match launchd_probe(host).map(|state| state.bootstrapped) {
            Some(true) => {
                launchd_command(host, &["bootout", &launchd_target(host)])?;
            }
            Some(false) => {}
            None if plist_path(&host.home()).is_file() => {
                return Err(
                    "could not ask launchd about the service, and it is still installed: \
                     refusing to reset while the indexer may be running."
                        .to_owned(),
                );
            }
            None => {}
        },
    }
    if host.daemon_lock_held()? {
        return Err(
            "refusing to reset while the indexer daemon is running: stop it first and try again."
                .to_owned(),
        );
    }
    Ok(())
}

fn query_systemd(host: &mut impl Host) -> ServiceState {
    let installed = unit_path(&host.config_dir()).is_file();
    if !installed {
        return ServiceState {
            installed: Some(false),
            enabled: None,
            running: None,
        };
    }
    ServiceState {
        installed: Some(true),
        enabled: systemd_enabled(host, crate::config::TS_SERVICE_NAME),
        running: match systemd_liveness(host, crate::config::TS_SERVICE_NAME, installed) {
            Liveness::Active | Liveness::Deactivating => Some(true),
            Liveness::Stopped => Some(false),
            Liveness::Unknown => None,
        },
    }
}

/// What `systemctl is-active` says about a unit: running (or heading
/// there), mid-stop, stopped, or unreadable. `deactivating` splits out
/// from the other transitional states because stop and start treat it
/// differently: a stop cancels the pending stop, but a start must push
/// through it — skipping it as "already running" would leave the service
/// stopped once the pending stop lands. `activating` and `reloading` end
/// running, so they read as active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Liveness {
    Active,
    Deactivating,
    Stopped,
    Unknown,
}

impl Liveness {
    /// Whether the unit might have a process to stop: anything but a
    /// confirmed stop. `deactivating` counts — a pending stop is
    /// cancelled by a stop, never skipped over — and so does an
    /// unreadable answer about an installed file.
    fn might_run(self) -> bool {
        !matches!(self, Self::Stopped)
    }
}

/// Reads a unit's liveness: `is-active` answers 0 for active (or
/// reloading) and 3 with a state word otherwise — except for a unit the
/// manager never loaded, which systemd 258 answers with exit 4 (and the
/// same `inactive` word older releases print with exit 3): confirmed
/// absent, with provably nothing to stop. Anything else (no user bus,
/// signals, no systemctl at all, an unrecognized word) means systemd
/// cannot say — except exit-3 `unknown` for a unit with no file
/// installed: older systemctl answers that for a unit it never loaded,
/// and with nothing installed nothing can run, so that reads as stopped
/// instead of failing closed. The caller passes whether the unit's file
/// exists; an `unknown` with a file present keeps failing closed, since a
/// lingering service may still run there.
fn systemd_liveness(host: &mut impl Host, unit: &str, file_present: bool) -> Liveness {
    let Ok(result) = host.run("systemctl", &["--user", "is-active", unit]) else {
        return Liveness::Unknown;
    };
    systemd_liveness_state(result.code, &result.stdout, file_present)
}

/// Maps one `systemctl is-active` answer to its liveness. Exit 0 is
/// running; exit 4 is confirmed-absent (see [`systemd_liveness`]); exit 3
/// consults the state word, since `activating` exits 3 yet must still be
/// stopped. Any other word (including `maintenance`) reads as unknown —
/// except `unknown` itself with no unit file installed, which is a
/// never-loaded unit and reads as stopped: only confirmed-absent states
/// skip the stop.
fn systemd_liveness_state(code: Option<i32>, stdout: &str, file_present: bool) -> Liveness {
    match code {
        Some(0) => Liveness::Active,
        Some(4) => Liveness::Stopped,
        Some(3) => match stdout.split_whitespace().next().unwrap_or("") {
            "inactive" | "failed" | "not-found" => Liveness::Stopped,
            "activating" | "reloading" => Liveness::Active,
            "deactivating" => Liveness::Deactivating,
            "unknown" if !file_present => Liveness::Stopped,
            _ => Liveness::Unknown,
        },
        _ => Liveness::Unknown,
    }
}

/// How `is-enabled` answers for a unit: persistently enabled, enabled
/// only until reboot, disabled in one of systemd's named ways, or
/// unreadable. Install needs the full answer — runtime-only enablement
/// would not survive reboot — while status only reports enabled or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Enablement {
    Enabled,
    EnabledRuntime,
    Disabled,
    Unknown,
}

/// Reads one unit's enablement from `is-enabled` stdout rather than its
/// exit code: the code cannot tell `disabled` from an unreadable answer.
/// Only a systemctl that cannot run at all reads as unknown.
fn systemd_enablement(host: &mut impl Host, unit: &str) -> Enablement {
    let Ok(result) = host.run("systemctl", &["--user", "is-enabled", unit]) else {
        return Enablement::Unknown;
    };
    systemd_enablement_state(&result.stdout)
}

/// Maps one `systemctl is-enabled` answer to its enablement. `enabled`
/// means persistently enabled; `enabled-runtime` only until reboot; every
/// other state systemd names for a present-but-not-enabled unit —
/// `disabled`, `static`, `masked`, `indirect`, `generated`, `bad`,
/// `alias` — means disabled, as does `not-found`: a unit the manager does
/// not know starts at no login. Anything else, including empty output on
/// failure, means systemd could not say. A `linked` unit reads as unknown
/// too: linking only puts the unit on the search path, and the answer
/// says nothing about whether the unit starts at login.
fn systemd_enablement_state(output: &str) -> Enablement {
    match output.split_whitespace().next().unwrap_or("") {
        "enabled" => Enablement::Enabled,
        "enabled-runtime" => Enablement::EnabledRuntime,
        "disabled" | "static" | "masked" | "masked-runtime" | "indirect" | "generated" | "bad"
        | "alias" | "not-found" => Enablement::Disabled,
        _ => Enablement::Unknown,
    }
}

/// Whether a unit is enabled, for `ts status`: runtime-only enablement
/// still counts — the service runs until reboot.
fn systemd_enabled(host: &mut impl Host, unit: &str) -> Option<bool> {
    match systemd_enablement(host, unit) {
        Enablement::Enabled | Enablement::EnabledRuntime => Some(true),
        Enablement::Disabled => Some(false),
        Enablement::Unknown => None,
    }
}

/// Runs a `systemctl --user` verb that must succeed, naming the verb and
/// the manager's own complaint on failure.
fn systemd_command(host: &mut impl Host, args: &[&str]) -> Result<(), String> {
    let mut full = vec!["--user"];
    full.extend(args.iter());
    let result = host.run("systemctl", &full)?;
    if result.success() {
        return Ok(());
    }
    Err(systemctl_error(&full, &result))
}

fn systemctl_error(args: &[&str], result: &CommandResult) -> String {
    let complaint = result.stderr.trim();
    if complaint.is_empty() {
        format!(
            "systemctl {} failed (exited with {})",
            args.join(" "),
            result
                .code
                .map_or("a signal".to_owned(), |code| code.to_string())
        )
    } else {
        format!("systemctl {} failed: {}", args.join(" "), complaint)
    }
}

/// Whether `executable` is an ephemeral copy no service file may point
/// at: under the temp directory, inside an AppImage mount, or deleted
/// while running (Linux suffixes `current_exe` with ` (deleted)`). The
/// mount pattern doubles the temp-dir check for AppImages launched with
/// a custom `TMPDIR`.
fn is_ephemeral_executable(executable: &Path, temp_dir: &Path) -> bool {
    if !temp_dir.as_os_str().is_empty() {
        if executable.starts_with(temp_dir) {
            return true;
        }
        // A symlinked temp dir defeats the lexical check when the
        // invocation spelling differs (macOS /var -> /private/var):
        // compare canonical paths as a fallback. Unresolvable paths
        // stay non-ephemeral — a missing executable fails elsewhere.
        if let (Ok(canonical), Ok(canonical_temp)) =
            (executable.canonicalize(), temp_dir.canonicalize())
        {
            if canonical.starts_with(canonical_temp) {
                return true;
            }
        }
    }
    let text = executable.to_string_lossy();
    text.starts_with("/tmp/.mount_") || text.ends_with(" (deleted)")
}

/// Refuses an `install` from an ephemeral executable: a service pointing
/// at an AppImage mount dies with the unmount, silently. Startup treats
/// the failure as degraded, so bare mounts run service-less but working.
fn refuse_ephemeral_executable(host: &impl Host) -> Result<(), String> {
    let executable = host.executable();
    if !is_ephemeral_executable(&executable, &host.temp_dir()) {
        return Ok(());
    }
    Err(format!(
        "cannot install a service from a temporary copy of ts ({}): install Toka first, then run `ts install` from the installed copy.",
        executable.display()
    ))
}

/// Attempts the `--link` step after the service setup, whatever its
/// outcome: a failed setup must not swallow the link, and a failed link
/// warns instead of failing an installed service — the link is the
/// explicitly optional part, and the installer restarts only on success.
/// Returns the service error (if any) with the combined outcome.
fn finish_install(
    host: &mut impl Host,
    outcome: &mut Outcome,
    service_error: Option<String>,
    link: bool,
) -> Result<Outcome, (String, Outcome)> {
    if link {
        if let Err(link_error) = link_ts(host, outcome) {
            outcome.already(format!("warning: {link_error}"));
        }
    }
    if let Some(error) = service_error {
        return Err((error, std::mem::take(outcome)));
    }
    Ok(std::mem::take(outcome))
}

fn install_systemd(host: &mut impl Host, link: bool) -> Result<Outcome, (String, Outcome)> {
    let mut outcome = Outcome::default();
    with_outcome(refuse_ephemeral_executable(host), &mut outcome)?;
    // Held for the whole install, link included: a second installer waits
    // here, then sees this one's converged state instead of interleaving
    // marker set/clear with it (see [`InstallLock`]).
    let _install_lock = with_outcome(
        acquire_install_lock(&unit_path(&host.config_dir())),
        &mut outcome,
    )?;
    // The service setup runs to its first error; the link below still
    // runs after a failure (see [`finish_install`]).
    let service_error = install_systemd_service(host, &mut outcome).err();
    finish_install(host, &mut outcome, service_error, link)
}

/// The systemd unit setup behind [`install_systemd`]: everything but the
/// ephemeral check, the install lock (held by the caller across the link
/// too), and the link itself. Stops at the first error; the caller still
/// attempts the link afterwards.
fn install_systemd_service(host: &mut impl Host, outcome: &mut Outcome) -> Result<(), String> {
    let home = host.home();
    let unit = unit_path(&host.config_dir());
    let name = crate::config::TS_SERVICE_NAME;

    // Read before the rewrite: the manager still reports the installed
    // executable, which the takeover note below compares against.
    let previous = previous_unit_executable(host, &unit, name);
    let rendered = render_unit(&host.executable(), &host.service_env())?;
    // The marker goes down before the write, so a crash anywhere past
    // here still retries the reload: file current plus marker set means
    // the manager may not have the file yet.
    let differs = service_file_differs(&unit, &rendered)?;
    if differs {
        set_reload_marker(&marker_path(&unit))?;
        write_service_file(&unit, &rendered)?;
        outcome.did(format!("wrote {}", display_path(&home, &unit)));
    } else {
        outcome.already("service file already up to date".to_owned());
    }
    // Last installer wins: a `ts` installed from another copy points the
    // service at itself, and says so. A reinstall of the same executable
    // stays quiet.
    if let Some(old) = &previous {
        if !same_executable(old, &host.executable()) {
            outcome.did(format!("taking over {name} from {}", old.display()));
        }
    }

    // The old service stops before anything new starts: it holds the
    // legacy daemon lock, and a new daemon started first would yield to
    // it and crash-loop under `Restart=on-failure`.
    let legacy_removed = migrate_legacy_service(host, outcome)?;

    // A changed, removed, or not-yet-loaded file needs a reload before
    // the stop below: stopping a unit the manager never loaded fails,
    // while past a reload the stop is a harmless no-op on a stopped
    // daemon.
    let marker = marker_path(&unit);
    let marker_set = marker.is_file();
    if differs || legacy_removed || marker_set {
        systemd_command(host, &["daemon-reload"])?;
        outcome.did("reloaded systemd".to_owned());
    }

    // Only persistent enablement converges: a runtime-only symlink would
    // not survive reboot, so it is re-enabled for good.
    if systemd_enablement(host, name) == Enablement::Enabled {
        outcome.already(format!("{name} is already enabled"));
    } else {
        systemd_command(host, &["enable", name])?;
        outcome.did(format!("enabled {name}"));
    }

    // A pending marker — or a stop that would land after the report —
    // reloads: stop (unless provably stopped) plus start, verified loaded
    // before the marker clears. Anything else converges cheaply: running
    // is already-held state, stopped starts, and unreadable starts
    // without claiming a state change.
    let liveness = systemd_liveness(host, name, unit.is_file());
    if marker_set || liveness == Liveness::Deactivating {
        let stopped = liveness == Liveness::Stopped;
        if !stopped {
            systemd_command(host, &["stop", name])?;
        }
        systemd_command(host, &["start", name])?;
        if stopped {
            outcome.did(format!("started {name}"));
        } else {
            outcome.did(format!("restarted {name}"));
        }
        verify_unit_loaded(host, name, &unit)?;
        clear_reload_marker(&marker)?;
    } else if liveness == Liveness::Active {
        outcome.already(format!("{name} is already running"));
    } else if liveness == Liveness::Stopped {
        systemd_command(host, &["start", name])?;
        outcome.did(format!("started {name}"));
    } else {
        // Unreadable probe: the start is still requested, but no state
        // change is claimed — the probe may have been interrupted while
        // active, in which case systemd accepts the request as a no-op.
        systemd_command(host, &["start", name])?;
        outcome.already(format!("systemd accepted the start request for {name}"));
    }

    ensure_linger(host, outcome);
    Ok(())
}

/// The executable the installed unit runs: the manager-side `ExecStart`,
/// read before an install rewrites the file, for the takeover note only.
/// `None` when no unit is installed, or when the manager cannot say what
/// it runs.
fn previous_unit_executable(host: &mut impl Host, unit: &Path, name: &str) -> Option<PathBuf> {
    if !unit.is_file() {
        return None;
    }
    let output = host
        .run("systemctl", &["--user", "show", "-p", "ExecStart", name])
        .ok()?;
    parse_service_executable(&output.stdout)
}

/// Whether a service file's disk content differs from the rendered one.
/// A missing file always differs; a symlink reads through to its target,
/// exactly as the write below compares.
fn service_file_differs(service_file: &Path, rendered: &str) -> Result<bool, String> {
    match std::fs::read_to_string(service_file) {
        Ok(existing) => Ok(existing != rendered),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!(
            "could not read {}: {error}",
            service_file.display()
        )),
    }
}

/// The pending-reload marker beside a service file: set before the file
/// is rewritten, cleared only after the reload is verified loaded. File
/// content plus marker is the one staleness mechanism on both managers —
/// it covers moved executables, settings-only changes, program-less
/// `launchctl print` output, and crashes between the write and the
/// reload, uniformly. Neither manager loads the marker: it is not a unit
/// or a plist, just a sibling file.
fn marker_path(service_file: &Path) -> PathBuf {
    let name = service_file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "service".to_owned());
    service_file.with_file_name(format!("{name}.pending-reload"))
}

/// Sets the pending-reload marker, creating its parent: the install
/// calls this before the write creates anything.
fn set_reload_marker(marker: &Path) -> Result<(), String> {
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    std::fs::write(marker, b"")
        .map_err(|error| format!("could not write {}: {error}", marker.display()))
}

/// Clears the pending-reload marker after a verified reload. A marker
/// that will not clear fails the install: silently leaving it would flap
/// the service with a redundant reload on every later install.
fn clear_reload_marker(marker: &Path) -> Result<(), String> {
    match std::fs::remove_file(marker) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove {}: {error}", marker.display())),
    }
}

/// The inter-process install lock beside a service file: held for the
/// whole install, from the first read to the last write. The marker alone
/// cannot serialize two installers — one sets it and writes its revision,
/// the other sets it and writes a newer one, then exits before its reload
/// succeeds, and the first clears the marker afterwards: the file holds
/// the new revision, the marker is gone, the service still runs the old
/// one, and a retry sees matching bytes with no marker and reports
/// convergence. With the lock a second installer waits here instead, then
/// sees the first installer's converged state. The lock file itself stays
/// behind, empty and inert; dropping the guard releases the lock.
#[cfg(unix)]
struct InstallLock {
    _file: std::fs::File,
}

/// No mutual exclusion where `flock` does not exist: service management
/// only ships on Unix, so this guard is a compile-time stand-in.
#[cfg(not(unix))]
struct InstallLock;

/// The lock file [`acquire_install_lock`] holds: beside the service file,
/// so installers for one service meet on one lock.
#[cfg(unix)]
fn install_lock_path(service_file: &Path) -> PathBuf {
    let name = service_file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "service".to_owned());
    service_file.with_file_name(format!("{name}.install.lock"))
}

/// Takes the install lock for `service_file`, waiting while another
/// installer holds it.
#[cfg(unix)]
fn acquire_install_lock(service_file: &Path) -> Result<InstallLock, String> {
    let path = install_lock_path(service_file);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| format!("could not lock {}: {error}", path.display()))?;
    use std::os::fd::AsRawFd;
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(InstallLock { _file: file });
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(format!("could not lock {}: {error}", path.display()));
        }
    }
}

/// Takes the install lock for `service_file` without waiting: `Ok(None)`
/// while another installer holds it. The serialization test's probe.
#[cfg(all(unix, test))]
fn try_acquire_install_lock(service_file: &Path) -> Result<Option<InstallLock>, String> {
    let path = install_lock_path(service_file);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| format!("could not lock {}: {error}", path.display()))?;
    use std::os::fd::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Ok(None);
        }
        return Err(format!("could not lock {}: {error}", path.display()));
    }
    Ok(Some(InstallLock { _file: file }))
}

#[cfg(not(unix))]
fn acquire_install_lock(_service_file: &Path) -> Result<InstallLock, String> {
    Ok(InstallLock)
}

/// Confirms the reloaded unit landed in the manager: anything but an
/// unreadable answer. Loaded-but-stopped counts — the manager has the
/// current file, and running-ness is runtime state `ts status` reports.
/// On failure the marker stays set, so the next install retries the
/// reload instead of reporting the stale daemon "already running".
fn verify_unit_loaded(host: &mut impl Host, name: &str, unit: &Path) -> Result<(), String> {
    match systemd_liveness(host, name, unit.is_file()) {
        Liveness::Active | Liveness::Deactivating | Liveness::Stopped => Ok(()),
        Liveness::Unknown => Err(format!(
            "could not verify {name} picked up the new service file; \
             the reload stays pending until the next install"
        )),
    }
}

/// Stops, disables, and removes the old `toka-indexer.service`, returning
/// whether its file was removed. A running old service that refuses to
/// stop fails the install: starting the new daemon next to it would only
/// watch it yield and crash-loop.
fn migrate_legacy_service(host: &mut impl Host, outcome: &mut Outcome) -> Result<bool, String> {
    let legacy = crate::config::INDEXER_SERVICE_NAME;
    let file = legacy_unit_path(&host.home());
    let present = file.is_file();
    let mut liveness = systemd_liveness(host, legacy, present);
    // An unreadable probe is indeterminate, never absence: the legacy file
    // may be gone while its daemon stays loaded, and skipping migration
    // would start the new service beside a daemon that still owns the lock
    // the new one needs. Retry once for a transient interruption, then
    // refuse before starting anything new.
    if liveness == Liveness::Unknown {
        liveness = systemd_liveness(host, legacy, present);
    }
    if liveness == Liveness::Unknown {
        return Err(format!(
            "could not ask systemd about the old {legacy}, and it may still be running: \
             refusing to install the new service beside it."
        ));
    }
    // No file and a confirmed stop: nothing to migrate.
    if !present && liveness == Liveness::Stopped {
        outcome.already(format!("no old {legacy} to migrate"));
        return Ok(false);
    }
    // The old daemon stops whenever it might run — an unreadable answer
    // stops too, since starting the new daemon next to a live old one
    // would only watch it yield and crash-loop.
    if liveness.might_run() {
        systemd_command(host, &["stop", legacy])?;
        outcome.did(format!("stopped the old {legacy}"));
    }
    if present {
        systemd_command(host, &["disable", legacy])?;
        outcome.did(format!("disabled the old {legacy}"));
        std::fs::remove_file(&file)
            .map_err(|error| format!("could not remove {}: {error}", file.display()))?;
        outcome.did(format!(
            "removed the old {}",
            display_path(&host.home(), &file)
        ));
        return Ok(true);
    }
    // No file, but the daemon ran: drop the dangling enablement, as an
    // uninstall does — a fileless unit left enabled would fail at login.
    if disable_if_enabled(host, legacy)? {
        outcome.did(format!("disabled the old {legacy}"));
    } else {
        outcome.already(format!("the old {legacy} is not enabled"));
    }
    Ok(false)
}

/// Drops a fileless unit's dangling enablement — but only when
/// `is-enabled` confirms some. A blind disable turns its
/// locale-specific "not found" into a hard error, which no stderr
/// sniffing can reliably recognize. Returns whether anything was
/// disabled, so callers report what they did.
fn disable_if_enabled(host: &mut impl Host, unit: &str) -> Result<bool, String> {
    match systemd_enablement(host, unit) {
        Enablement::Enabled | Enablement::EnabledRuntime => {
            systemd_command(host, &["disable", unit])?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Lets the service outlive the login session. Best effort only: without
/// lingering the install is still complete, it just stops at logout — so a
/// refusal guides instead of failing.
fn ensure_linger(host: &mut impl Host, outcome: &mut Outcome) {
    let Some(user) = host.user() else {
        outcome.already(
            "could not determine the login name: run `loginctl enable-linger <you>` \
             so indexing continues while logged out"
                .to_owned(),
        );
        return;
    };
    let lingering = host
        .run("loginctl", &["show-user", &user, "-p", "Linger", "--value"])
        .map(|result| result.success() && result.stdout.trim() == "yes")
        .unwrap_or(false);
    if lingering {
        outcome.already("login lingering already enabled".to_owned());
        return;
    }
    let enabled = host
        .run("loginctl", &["enable-linger", &user])
        .map(|result| result.success())
        .unwrap_or(false);
    if enabled {
        outcome.did("enabled login lingering".to_owned());
    } else {
        outcome.already(format!(
            "could not enable login lingering: run `loginctl enable-linger {user}` \
             so indexing continues while logged out"
        ));
    }
}

fn uninstall_systemd(host: &mut impl Host, purge_data: bool) -> Result<Outcome, (String, Outcome)> {
    let mut outcome = Outcome::default();
    let home = host.home();
    let config = host.config_dir();
    // One probe per unit threaded through both phases, as on launchd: a
    // second probe after the pre-stop would see a manager that acks stop
    // without stopping as still running, and stop — and report — twice.
    let mut units = [
        PreStop::probe(
            host,
            crate::config::TS_SERVICE_NAME,
            unit_path(&config),
            true,
        ),
        PreStop::probe(
            host,
            crate::config::INDEXER_SERVICE_NAME,
            legacy_unit_path(&home),
            false,
        ),
    ];
    if purge_data {
        // Stops before the lock check: a service daemon is stopped, not
        // refused — only a hand-started survivor blocks the purge. No
        // deletion has happened yet, so a refusal removes nothing.
        for probed in &mut units {
            // No file and the manager cannot say: proceed, like launchd —
            // the lock check below still refuses while a live daemon holds
            // the lock.
            if probed.liveness == Liveness::Unknown && !probed.present {
                continue;
            }
            if probed.liveness.might_run() {
                with_outcome(systemd_command(host, &["stop", probed.unit]), &mut outcome)?;
                outcome.did(format!("stopped {}", probed.unit));
                probed.stopped = true;
            }
        }
        refuse_purge_if_locked(host, &mut outcome)?;
    }
    let mut removed_file = false;
    for probed in &units {
        removed_file |= with_outcome(
            remove_systemd_service(host, &mut outcome, probed),
            &mut outcome,
        )?;
    }
    if removed_file {
        with_outcome(systemd_command(host, &["daemon-reload"]), &mut outcome)?;
        outcome.did("reloaded systemd".to_owned());
    }
    with_outcome(unlink_ts(host, &mut outcome), &mut outcome)?;
    if purge_data {
        purge_index_data(host, &mut outcome)?;
    }
    Ok(outcome)
}

/// Refuses a purge while a supervisor holds the daemon lock: the stops
/// above only reach managed daemons, and a hand-started one would watch
/// its index vanish.
fn refuse_purge_if_locked(
    host: &mut impl Host,
    outcome: &mut Outcome,
) -> Result<(), (String, Outcome)> {
    if with_outcome(host.daemon_lock_held(), outcome)? {
        return Err((
            "refusing to purge index data while the indexer daemon is running: \
             stop it first and try again."
                .to_owned(),
            std::mem::take(outcome),
        ));
    }
    Ok(())
}

/// One unit's uninstall probe: its file, whether it is installed, what
/// the manager said about it, and whether the purge pre-stop already
/// stopped it. Probed once up front and threaded through both uninstall
/// phases, as on launchd.
struct PreStop {
    unit: &'static str,
    file: PathBuf,
    report_absent: bool,
    present: bool,
    liveness: Liveness,
    stopped: bool,
}

impl PreStop {
    fn probe(host: &mut impl Host, unit: &'static str, file: PathBuf, report_absent: bool) -> Self {
        let present = file.is_file();
        let liveness = systemd_liveness(host, unit, present);
        Self {
            unit,
            file,
            report_absent,
            present,
            liveness,
            stopped: false,
        }
    }
}

/// Stops, disables, and removes one unit, returning whether its file was
/// removed. The daemon stops whenever it might run — including when
/// systemd cannot say about an installed file — and a stop failure aborts
/// before anything is deleted. A fileless unit keeps its own path (see
/// [`remove_fileless_systemd_service`]). The probe is threaded in from the
/// uninstall's single pass, never re-read: a unit the pre-stop already
/// stopped is not stopped again.
fn remove_systemd_service(
    host: &mut impl Host,
    outcome: &mut Outcome,
    probed: &PreStop,
) -> Result<bool, String> {
    if !probed.present {
        return remove_fileless_systemd_service(host, outcome, probed);
    }
    let unit = probed.unit;
    let file = &probed.file;
    if probed.liveness.might_run() && !probed.stopped {
        systemd_command(host, &["stop", unit])?;
        outcome.did(format!("stopped {unit}"));
    }
    systemd_command(host, &["disable", unit])?;
    outcome.did(format!("disabled {unit}"));
    std::fs::remove_file(file)
        .map_err(|error| format!("could not remove {}: {error}", file.display()))?;
    outcome.did(format!("removed {}", display_path(&host.home(), file)));
    // A failed install's leftover marker goes with the unit: without
    // the service it is inert litter, and the next install sets its
    // own. Best effort — the uninstall already converged.
    let _ = std::fs::remove_file(marker_path(file));
    Ok(true)
}

/// Stops and disables one unit with no file under the config dir,
/// returning false — there is never a file to remove. The daemon stops
/// from the FIRST probe whenever it might run, unless the purge pre-stop
/// already stopped it; with no file installed and no answer from the
/// manager there is nothing to stop, so the uninstall proceeds like
/// launchd's instead of failing on a stop. Enablement is then decided on
/// its own probe, independently of the stop: a unit the manager loads
/// from outside the config dir (say its `$XDG_DATA_HOME`) is fileless
/// here yet enabled, and skipping the disable would restart it at the
/// next login. The `ts` unit reports its own absence; the legacy one
/// stays silent when there was never anything to migrate.
fn remove_fileless_systemd_service(
    host: &mut impl Host,
    outcome: &mut Outcome,
    probed: &PreStop,
) -> Result<bool, String> {
    let unit = probed.unit;
    let liveness = probed.liveness;
    if liveness != Liveness::Unknown && liveness.might_run() && !probed.stopped {
        systemd_command(host, &["stop", unit])?;
        outcome.did(format!("stopped {unit}"));
    }
    if disable_if_enabled(host, unit)? {
        outcome.did(format!("disabled {unit}"));
        return Ok(false);
    }
    if liveness == Liveness::Stopped || liveness == Liveness::Unknown {
        if probed.report_absent {
            outcome.already(format!("{unit} is not installed"));
        }
    } else {
        outcome.already(format!("{unit} is not enabled"));
    }
    Ok(false)
}

fn start_systemd(host: &mut impl Host) -> Result<Outcome, (String, Outcome)> {
    let mut outcome = Outcome::default();
    let unit = crate::config::TS_SERVICE_NAME;
    let installed = unit_path(&host.config_dir()).is_file();
    if !installed {
        return Err((
            format!("{unit} is not installed; run `ts install` first."),
            outcome,
        ));
    }
    match systemd_liveness(host, unit, installed) {
        Liveness::Active => {
            outcome.already(format!("{unit} is already running"));
        }
        // Mid-stop is actionable but never skippable: the stop lands the
        // pending stop, then the start brings the service back.
        Liveness::Deactivating => {
            with_outcome(systemd_command(host, &["stop", unit]), &mut outcome)?;
            outcome.did(format!("stopped {unit}"));
            with_outcome(systemd_command(host, &["start", unit]), &mut outcome)?;
            outcome.did(format!("started {unit}"));
        }
        // An unreadable state errors like restart's: starting blind
        // would report `started` even over an already-running daemon.
        Liveness::Unknown => {
            return Err((
                "could not ask systemd about the service.".to_owned(),
                outcome,
            ));
        }
        Liveness::Stopped => {
            with_outcome(systemd_command(host, &["start", unit]), &mut outcome)?;
            outcome.did(format!("started {unit}"));
        }
    }
    Ok(outcome)
}

fn stop_systemd(host: &mut impl Host) -> Result<Outcome, (String, Outcome)> {
    let mut outcome = Outcome::default();
    let unit = crate::config::TS_SERVICE_NAME;
    let installed = unit_path(&host.config_dir()).is_file();
    match systemd_liveness(host, unit, installed) {
        Liveness::Active | Liveness::Deactivating => {
            with_outcome(systemd_command(host, &["stop", unit]), &mut outcome)?;
            outcome.did(format!("stopped {unit}"));
        }
        Liveness::Stopped if installed => {
            outcome.already(format!("{unit} is already stopped"));
        }
        // Like launchd: an installed service systemd cannot report on is
        // an error, not an "already stopped" — stopping blind would claim
        // convergence it never checked.
        Liveness::Unknown if installed => {
            return Err((
                "could not ask systemd about the service.".to_owned(),
                outcome,
            ));
        }
        _ => {
            outcome.already(format!("{unit} is not installed, nothing to stop"));
        }
    }
    Ok(outcome)
}

fn restart_systemd(host: &mut impl Host) -> Result<Outcome, (String, Outcome)> {
    let mut outcome = Outcome::default();
    let unit = crate::config::TS_SERVICE_NAME;
    let installed = unit_path(&host.config_dir()).is_file();
    if !installed {
        return Err((
            format!("{unit} is not installed; run `ts install` first."),
            outcome,
        ));
    }
    // Stop plus start, not `systemctl restart`: each half reports what it
    // did, and a stopped service still ends up running. The new daemon
    // process publishes a fresh start time, so `ts status` uptime resets.
    // An unreadable state errors like launchd's restart: starting without
    // stopping would silently no-op while a stale daemon runs.
    match systemd_liveness(host, unit, installed) {
        Liveness::Active | Liveness::Deactivating => {
            with_outcome(systemd_command(host, &["stop", unit]), &mut outcome)?;
            outcome.did(format!("stopped {unit}"));
        }
        Liveness::Stopped => {
            outcome.already(format!("{unit} is already stopped"));
            // Still a real restart: the stop is a verified no-op on a
            // truly-stopped unit, and it kills a stale daemon the probe
            // missed — a bare start would no-op while it kept running.
            with_outcome(systemd_command(host, &["stop", unit]), &mut outcome)?;
        }
        Liveness::Unknown => {
            return Err((
                "could not ask systemd about the service.".to_owned(),
                outcome,
            ));
        }
    }
    with_outcome(systemd_command(host, &["start", unit]), &mut outcome)?;
    outcome.did(format!("started {unit}"));
    Ok(outcome)
}

/// The user's GUI domain service target, e.g. `gui/501/app.toka.ts`.
fn launchd_target(host: &impl Host) -> String {
    format!("gui/{}/{}", host.uid(), crate::config::TS_LAUNCHD_LABEL)
}

fn query_launchd(host: &mut impl Host) -> ServiceState {
    if !plist_path(&host.home()).is_file() {
        return ServiceState {
            installed: Some(false),
            enabled: None,
            running: None,
        };
    }
    // launchd has no enabled bit of its own for agents: bootstrapped is
    // loaded, and a loaded agent with `RunAtLoad` runs. `launchctl print`
    // answers both at once — exit 0 means bootstrapped, and its `state`
    // line says whether the job runs right now.
    match launchd_probe(host) {
        None => ServiceState {
            installed: Some(true),
            enabled: None,
            running: None,
        },
        Some(state) => ServiceState {
            installed: Some(true),
            enabled: Some(state.bootstrapped),
            running: state.bootstrapped.then_some(state.running).flatten(),
        },
    }
}

/// One `launchctl print`: whether the agent is bootstrapped and whether
/// its job runs. `None` is a launchctl that cannot be trusted — missing,
/// signal-killed, or failing for any reason but a confirmed-absent job.
/// An agent that is not bootstrapped may still have a hand-started daemon
/// behind it, so its running answer stays unknown rather than false. The
/// loaded executable is deliberately not read: staleness is the marker's
/// job, so program-less `print` output needs no special case.
struct LaunchdState {
    bootstrapped: bool,
    running: Option<bool>,
}

fn launchd_probe(host: &mut impl Host) -> Option<LaunchdState> {
    let target = launchd_target(host);
    let result = host.run("launchctl", &["print", &target]).ok()?;
    parse_launchd_print(&result)
}

/// Maps one `launchctl print` answer to loaded, absent, or unknown. Exit 0
/// is loaded; any other exit is absent ONLY when launchctl confirms the
/// job is missing — any other failure (a signal, an unexpected complaint)
/// reads as unknown, never as safe-to-delete.
fn parse_launchd_print(result: &CommandResult) -> Option<LaunchdState> {
    if !result.success() {
        if result.code.is_some() && result.stderr.contains("Could not find service") {
            return Some(LaunchdState {
                bootstrapped: false,
                running: None,
            });
        }
        return None;
    }
    Some(LaunchdState {
        bootstrapped: true,
        running: launchd_running_state(&result.stdout),
    })
}

fn launchd_bootstrapped(host: &mut impl Host) -> Option<bool> {
    launchd_probe(host).map(|state| state.bootstrapped)
}

/// One `key = value` field from `launchctl print` output. The key must
/// match exactly — a bare prefix would also match a hypothetical
/// `statements =`.
fn launchd_print_field(print_output: &str, key: &str) -> Option<String> {
    for line in print_output.lines() {
        let line = line.trim();
        let Some((found, value)) = line.split_once('=') else {
            continue;
        };
        if found.trim() != key {
            continue;
        }
        return Some(value.trim().trim_end_matches(';').trim().to_owned());
    }
    None
}

/// Pulls the job state out of `launchctl print` output, which carries a
/// `state = running` line for a live job (`exited`, `spawn scheduled`,
/// and the rest read as not running). No state line means launchd would
/// not say.
fn launchd_running_state(print_output: &str) -> Option<bool> {
    launchd_print_field(print_output, "state").map(|state| state == "running")
}

/// Runs a `launchctl` verb that must succeed, naming the verb and the
/// manager's own complaint on failure.
fn launchd_command(host: &mut impl Host, args: &[&str]) -> Result<(), String> {
    let result = host.run("launchctl", args)?;
    if result.success() {
        return Ok(());
    }
    let complaint = result.stderr.trim();
    if complaint.is_empty() {
        Err(format!(
            "launchctl {} failed (exited with {})",
            args.join(" "),
            result
                .code
                .map_or("a signal".to_owned(), |code| code.to_string())
        ))
    } else {
        Err(format!(
            "launchctl {} failed: {}",
            args.join(" "),
            complaint
        ))
    }
}

fn install_launchd(host: &mut impl Host, link: bool) -> Result<Outcome, (String, Outcome)> {
    let mut outcome = Outcome::default();
    with_outcome(refuse_ephemeral_executable(host), &mut outcome)?;
    // Held for the whole install, link included, as on systemd (see
    // [`InstallLock`]).
    let _install_lock = with_outcome(
        acquire_install_lock(&plist_path(&host.home())),
        &mut outcome,
    )?;
    // The agent setup runs to its first error; the link below still runs
    // after a failure (see [`finish_install`]).
    let service_error = install_launchd_service(host, &mut outcome).err();
    finish_install(host, &mut outcome, service_error, link)
}

/// The LaunchAgent setup behind [`install_launchd`]: everything but the
/// ephemeral check, the install lock (held by the caller across the link
/// too), and the link itself. Stops at the first error; the caller still
/// attempts the link afterwards.
fn install_launchd_service(host: &mut impl Host, outcome: &mut Outcome) -> Result<(), String> {
    let home = host.home();
    let plist = plist_path(&home);
    let label = crate::config::TS_LAUNCHD_LABEL;

    // Read before the rewrite: the installed executable, which the
    // takeover note below compares against.
    let previous = std::fs::read_to_string(&plist)
        .ok()
        .and_then(|contents| parse_plist_executable(&contents));
    let rendered = render_plist(&host.executable(), &host.service_env());
    // The marker goes down before the write, as on systemd, so a crash
    // anywhere past here still retries the reload.
    let differs = service_file_differs(&plist, &rendered)?;
    if differs {
        set_reload_marker(&marker_path(&plist))?;
        write_service_file(&plist, &rendered)?;
        outcome.did(format!("wrote {}", display_path(&home, &plist)));
    } else {
        outcome.already("service file already up to date".to_owned());
    }
    // Last installer wins, as on systemd: installing from another copy
    // points the agent at itself, and says so.
    if let Some(old) = previous {
        if !same_executable(&old, &host.executable()) {
            outcome.did(format!("taking over {label} from {}", old.display()));
        }
    }

    let Some(state) = launchd_probe(host) else {
        return Err("could not ask launchd about the service.".to_owned());
    };
    let bootstrapped = state.bootstrapped;
    let running = state.running;
    let marker = marker_path(&plist);
    // A pending marker — or an agent launchd never loaded — reloads:
    // unload and load again so the new one takes effect now, not at the
    // next login. Anything else converges cheaply: a loaded current agent
    // is already running, kickstarted, or already loaded.
    if marker.is_file() || !bootstrapped {
        if bootstrapped {
            launchd_command(host, &["bootout", &launchd_target(host)])?;
            outcome.did(format!("stopped {label}"));
        }
        launchd_command(
            host,
            &[
                "bootstrap",
                &format!("gui/{}", host.uid()),
                &plist_display(&plist),
            ],
        )?;
        outcome.did(format!("started {label}"));
        verify_agent_bootstrapped(host, label)?;
        clear_reload_marker(&marker)?;
    } else {
        match running {
            Some(true) => outcome.already(format!("{label} is already running")),
            Some(false) => {
                launchd_command(host, &["kickstart", "-k", &launchd_target(host)])?;
                outcome.did(format!("started {label}"));
            }
            None => outcome.already(format!("{label} is already loaded")),
        }
    }
    Ok(())
}

/// The executable a LaunchAgent plist runs: the first `ProgramArguments`
/// string, with [`xml_escape`] reversed. `None` when the plist names none.
fn parse_plist_executable(plist: &str) -> Option<PathBuf> {
    let marker = "<key>ProgramArguments</key>";
    let rest = &plist[plist.find(marker)? + marker.len()..];
    let open = "<string>";
    let start = rest.find(open)? + open.len();
    let end = rest[start..].find("</string>")? + start;
    let path = xml_unescape(&rest[start..end]);
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Reverses [`xml_escape`]: the `&amp;` last, so `&amp;lt;` unescapes to
/// `&lt;` rather than `<`.
fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The plist path as `launchctl bootstrap` takes it: absolute, as written.
/// Lossy on purpose, like every path this CLI handles: a non-UTF-8 home
/// must not break the invocation.
fn plist_display(plist: &Path) -> String {
    plist.to_string_lossy().into_owned()
}

/// Confirms the reloaded agent landed in launchd: bootstrapped, as the
/// bootstrap just reported. On failure the marker stays set, so the next
/// install retries the reload instead of reporting the stale agent
/// "already running".
fn verify_agent_bootstrapped(host: &mut impl Host, label: &str) -> Result<(), String> {
    match launchd_bootstrapped(host) {
        Some(true) => Ok(()),
        _ => Err(format!(
            "could not verify {label} picked up the new agent; \
             the reload stays pending until the next install"
        )),
    }
}

fn uninstall_launchd(host: &mut impl Host, purge_data: bool) -> Result<Outcome, (String, Outcome)> {
    let mut outcome = Outcome::default();
    let home = host.home();
    let plist = plist_path(&home);
    let label = crate::config::TS_LAUNCHD_LABEL;
    let present = plist.is_file();

    // One probe threaded through both phases: a second probe after the
    // pre-stop would see a manager that ignores bootout as still loaded
    // and stop — and report — twice.
    let bootstrapped = launchd_bootstrapped(host);
    let mut stopped = false;
    if purge_data {
        // A bootout before the lock check, mirroring systemd: a managed
        // daemon is stopped, not refused. An unreadable answer is left to
        // the main flow, which errors before deleting anything.
        if bootstrapped == Some(true) {
            with_outcome(
                launchd_command(host, &["bootout", &launchd_target(host)]),
                &mut outcome,
            )?;
            outcome.did(format!("stopped {label}"));
            stopped = true;
        }
        refuse_purge_if_locked(host, &mut outcome)?;
    }

    match bootstrapped {
        None if !present => {
            outcome.already(format!("{label} is not installed"));
        }
        None => {
            return Err((
                "could not ask launchd about the service.".to_owned(),
                std::mem::take(&mut outcome),
            ));
        }
        Some(was_bootstrapped) => {
            if was_bootstrapped && !stopped {
                with_outcome(
                    launchd_command(host, &["bootout", &launchd_target(host)]),
                    &mut outcome,
                )?;
                outcome.did(format!("stopped {label}"));
            }
            if present {
                with_outcome(
                    std::fs::remove_file(&plist)
                        .map_err(|error| format!("could not remove {}: {error}", plist.display())),
                    &mut outcome,
                )?;
                outcome.did(format!("removed {}", display_path(&home, &plist)));
                // A failed install's leftover marker goes with the plist,
                // as on systemd. Best effort — the uninstall converged.
                let _ = std::fs::remove_file(marker_path(&plist));
            }
            if !was_bootstrapped && !present {
                outcome.already(format!("{label} is not installed"));
            }
        }
    }
    with_outcome(unlink_ts(host, &mut outcome), &mut outcome)?;
    if purge_data {
        purge_index_data(host, &mut outcome)?;
    }
    Ok(outcome)
}

fn start_launchd(host: &mut impl Host) -> Result<Outcome, (String, Outcome)> {
    let mut outcome = Outcome::default();
    let label = crate::config::TS_LAUNCHD_LABEL;
    if !plist_path(&host.home()).is_file() {
        return Err((
            format!("{label} is not installed; run `ts install` first."),
            outcome,
        ));
    }
    let Some(state) = launchd_probe(host) else {
        return Err((
            "could not ask launchd about the service.".to_owned(),
            outcome,
        ));
    };
    let bootstrapped = state.bootstrapped;
    let running = state.running;
    if running == Some(true) {
        outcome.already(format!("{label} is already running"));
    } else if bootstrapped {
        with_outcome(
            launchd_command(host, &["kickstart", "-k", &launchd_target(host)]),
            &mut outcome,
        )?;
        outcome.did(format!("started {label}"));
    } else {
        with_outcome(
            launchd_command(
                host,
                &[
                    "bootstrap",
                    &format!("gui/{}", host.uid()),
                    &plist_display(&plist_path(&host.home())),
                ],
            ),
            &mut outcome,
        )?;
        outcome.did(format!("started {label}"));
    }
    Ok(outcome)
}

fn stop_launchd(host: &mut impl Host) -> Result<Outcome, (String, Outcome)> {
    let mut outcome = Outcome::default();
    let label = crate::config::TS_LAUNCHD_LABEL;
    let installed = plist_path(&host.home()).is_file();
    match launchd_probe(host) {
        None if !installed => {
            outcome.already(format!("{label} is not installed, nothing to stop"));
        }
        None => {
            return Err((
                "could not ask launchd about the service.".to_owned(),
                outcome,
            ));
        }
        Some(state) => {
            if state.bootstrapped {
                with_outcome(
                    launchd_command(host, &["bootout", &launchd_target(host)]),
                    &mut outcome,
                )?;
                outcome.did(format!("stopped {label}"));
            } else if installed {
                outcome.already(format!("{label} is already stopped"));
            } else {
                outcome.already(format!("{label} is not installed, nothing to stop"));
            }
        }
    }
    Ok(outcome)
}

fn restart_launchd(host: &mut impl Host) -> Result<Outcome, (String, Outcome)> {
    let mut outcome = Outcome::default();
    let label = crate::config::TS_LAUNCHD_LABEL;
    let home = host.home();
    if !plist_path(&home).is_file() {
        return Err((
            format!("{label} is not installed; run `ts install` first."),
            outcome,
        ));
    }
    // Stop plus start, mirroring the systemd flow: each half reports what
    // it did, a stopped agent still ends up running, and the new daemon
    // process resets `ts status` uptime.
    let Some(state) = launchd_probe(host) else {
        return Err((
            "could not ask launchd about the service.".to_owned(),
            outcome,
        ));
    };
    if state.bootstrapped {
        with_outcome(
            launchd_command(host, &["bootout", &launchd_target(host)]),
            &mut outcome,
        )?;
        outcome.did(format!("stopped {label}"));
    } else {
        // No bootout here, unlike systemd's restart: bootout on an agent
        // that is not bootstrapped fails, and nothing loaded means
        // nothing manager-run to stop — only the bootstrap below.
        outcome.already(format!("{label} is already stopped"));
    }
    with_outcome(
        launchd_command(
            host,
            &[
                "bootstrap",
                &format!("gui/{}", host.uid()),
                &plist_display(&plist_path(&home)),
            ],
        ),
        &mut outcome,
    )?;
    outcome.did(format!("started {label}"));
    Ok(outcome)
}

/// Writes a service file, creating its parent. An identical file is left
/// alone, so a second install reports already-held state instead of
/// churning mtimes; the return says whether anything was written. The
/// replacement is atomic: the new content lands in a temporary sibling
/// first and is renamed over the old file, so a full filesystem leaves
/// the previous config intact instead of a truncation.
fn write_service_file(path: &Path, contents: &str) -> Result<bool, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    match std::fs::read_to_string(path) {
        Ok(existing) if existing == contents => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("could not read {}: {error}", path.display()));
        }
    }
    // Never write through a symlink: a dotfiles-managed unit would
    // clobber its target's contents. The replacement is staged first and
    // the final rename swaps the link itself out — the link is never
    // removed before its replacement exists, so a failed staging leaves
    // the old link in place instead of no file at all.
    let sibling = sibling_temp_file(path);
    if let Err(error) = std::fs::write(&sibling, contents) {
        let _ = std::fs::remove_file(&sibling);
        return Err(format!("could not write {}: {error}", path.display()));
    }
    // Windows cannot rename over an existing file; service files only
    // ship on Unix, but the fallback keeps the helper portable.
    #[cfg(windows)]
    let _ = std::fs::remove_file(path);
    if let Err(error) = std::fs::rename(&sibling, path) {
        let _ = std::fs::remove_file(&sibling);
        return Err(format!("could not write {}: {error}", path.display()));
    }
    Ok(true)
}

/// The temporary sibling [`write_service_file`] stages content in: beside
/// the target so the rename stays on one filesystem, process-tagged so
/// two concurrent installs never share it.
fn sibling_temp_file(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "service".to_owned());
    path.with_file_name(format!("{name}.tmp-{}", std::process::id()))
}

/// Links `ts` into `~/.local/bin`, so install scripts and shells find the
/// same executable the service runs. A link that already points here is
/// left alone; a stale link is replaced. Anything else at that path — a
/// directory, or a real copied binary — errors instead of being deleted.
fn link_ts(host: &mut impl Host, outcome: &mut Outcome) -> Result<(), String> {
    let home = host.home();
    let link = link_path(&home);
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    if link_points_at(&link, &host.executable()) {
        outcome.already(format!("{} is already linked", display_path(&home, &link)));
        return Ok(());
    }
    // The destination already IS this executable — a copy installed as
    // `~/.local/bin/ts` itself. Deleting it to symlink the path to
    // itself would only manufacture an ELOOP, so it stays.
    if link_is_same_file(&link, &host.executable()) {
        outcome.already(format!(
            "{} is already the ts binary",
            display_path(&home, &link)
        ));
        return Ok(());
    }
    match std::fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(format!(
                "could not link {}: a directory is in the way",
                display_path(&home, &link)
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            std::fs::remove_file(&link)
                .map_err(|error| format!("could not replace {}: {error}", link.display()))?;
        }
        Ok(_) => {
            return Err(format!(
                "could not link {}: a regular file is in the way; remove it first",
                display_path(&home, &link)
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!("could not read {}: {error}", link.display()));
        }
    }
    create_link(&host.executable(), &link)
        .map_err(|error| format!("could not link {}: {error}", link.display()))?;
    outcome.did(format!("linked {}", display_path(&home, &link)));
    Ok(())
}

#[cfg(unix)]
fn create_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn create_link(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "linking is only supported on Unix",
    ))
}

/// Whether `link` is a symlink resolving to `executable`. A relative
/// link resolves against its own parent, not the working directory.
fn link_points_at(link: &Path, executable: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return false;
    };
    let target = if target.is_absolute() {
        target
    } else if let Some(parent) = link.parent() {
        parent.join(target)
    } else {
        target
    };
    same_executable(&target, executable)
}

/// Whether `link` already is `executable` itself: the same path, or —
/// on Unix — the same inode under another name (a hard link). A symlink
/// to the executable answers true too, but the caller checks links
/// first and reports those separately.
fn link_is_same_file(link: &Path, executable: &Path) -> bool {
    if same_executable(link, executable) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(link), std::fs::metadata(executable)) {
            (Ok(link), Ok(executable)) => {
                link.dev() == executable.dev() && link.ino() == executable.ino()
            }
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Whether two executable paths name the same file: canonical paths
/// compared, falling back to the raw spellings when either side cannot
/// resolve. Shared by `toka restart`'s per-executable guard and `ts
/// install`'s takeover check, so a worktree copy never silently hijacks
/// another copy's service.
pub fn same_executable(left: &Path, right: &Path) -> bool {
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

/// Removes the `~/.local/bin/ts` link, but only when it points at this
/// executable: a link to another `ts` (or anything else) is left alone,
/// silently.
fn unlink_ts(host: &mut impl Host, outcome: &mut Outcome) -> Result<(), String> {
    let home = host.home();
    let link = link_path(&home);
    if !link_points_at(&link, &host.executable()) {
        return Ok(());
    }
    std::fs::remove_file(&link)
        .map_err(|error| format!("could not remove {}: {error}", link.display()))?;
    outcome.did(format!("removed {}", display_path(&home, &link)));
    Ok(())
}

/// Removes the index data the host names: the index folder, the logs, the
/// thumbnail cache. Missing paths are skipped silently, exactly as a reset
/// skips them; anything left behind fails, naming what resisted. The lock
/// is checked again here — not just before the uninstall started — so a
/// daemon started mid-uninstall still blocks the deletion.
fn purge_index_data(host: &mut impl Host, outcome: &mut Outcome) -> Result<(), (String, Outcome)> {
    refuse_purge_if_locked(host, outcome)?;
    let home = host.home();
    let paths = with_outcome(host.purge_paths(), outcome)?;
    let plan = crate::reset::ResetPlan::new(paths);
    let report = crate::reset::apply(&plan);
    for removed in &report.removed {
        outcome.did(format!("removed {}", display_path(&home, removed)));
    }
    if !report.failures.is_empty() {
        return Err((report.failures.join("\n"), std::mem::take(outcome)));
    }
    if report.removed.is_empty() {
        outcome.already("no index data to remove".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reset::ResetTarget;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;

    /// The scripted manager state behind a [`Fake`]: which units are
    /// active/enabled, whether the agent is bootstrapped and running,
    /// whether lingering is on, and which invocations fail.
    #[derive(Default)]
    struct Script {
        active: HashMap<String, bool>,
        active_unknown: HashSet<String>,
        activating: HashSet<String>,
        /// A pinned exit-3 state word per unit, for answers no other
        /// script field expresses: real `unknown`, or a localized word.
        /// Wins over every other liveness field.
        active_word: HashMap<String, String>,
        enabled: HashMap<String, bool>,
        enabled_runtime: HashSet<String>,
        bootstrapped: bool,
        job_running: bool,
        /// The loaded agent's executable, printed as `program = ...`. `None`
        /// omits the line, as older `launchctl print` output does.
        program: Option<String>,
        /// A `bootout` that reports success without unloading, for a
        /// manager that ignores the request.
        bootout_noop: bool,
        /// A `stop` that reports success without stopping, for a manager
        /// that ignores the request.
        stop_noop: bool,
        lingering: bool,
        fail: HashSet<String>,
        fail_stderr: HashMap<String, String>,
        missing: HashSet<String>,
    }

    impl Script {
        fn answer(&mut self, program: &str, args: &[&str]) -> Result<CommandResult, String> {
            if self.missing.contains(program) {
                return Err(format!("could not run {program}"));
            }
            let call = format!("{program} {}", args.join(" "));
            if self.fail.contains(&call) {
                let stderr = self
                    .fail_stderr
                    .get(&call)
                    .cloned()
                    .unwrap_or_else(|| "injected failure".to_owned());
                return Ok(CommandResult {
                    code: Some(1),
                    stdout: String::new(),
                    stderr,
                });
            }
            match program {
                "systemctl" => Ok(self.answer_systemctl(args)),
                "launchctl" => Ok(self.answer_launchctl(args)),
                "loginctl" => Ok(self.answer_loginctl(args)),
                _ => panic!("unexpected program in test: {call}"),
            }
        }

        fn answer_systemctl(&mut self, args: &[&str]) -> CommandResult {
            let out = |code: i32, stdout: &str| CommandResult {
                code: Some(code),
                stdout: stdout.to_owned(),
                stderr: String::new(),
            };
            match args {
                ["--user", "is-active", unit] => {
                    if let Some(word) = self.active_word.get(*unit) {
                        out(3, &format!("{word}\n"))
                    } else if self.active_unknown.contains(*unit) {
                        // A signal-killed probe: no exit code at all.
                        CommandResult {
                            code: None,
                            stdout: String::new(),
                            stderr: String::new(),
                        }
                    } else if self.activating.contains(*unit) {
                        out(3, "activating\n")
                    } else if self.active.get(*unit) == Some(&true) {
                        out(0, "active\n")
                    } else {
                        out(3, "inactive\n")
                    }
                }
                ["--user", "is-enabled", unit] => {
                    if self.enabled.get(*unit) == Some(&true) {
                        out(0, "enabled\n")
                    } else if self.enabled_runtime.contains(*unit) {
                        out(0, "enabled-runtime\n")
                    } else {
                        out(1, "disabled\n")
                    }
                }
                ["--user", "start", unit] => {
                    self.active.insert((*unit).to_owned(), true);
                    out(0, "")
                }
                ["--user", "stop", unit] => {
                    if !self.stop_noop {
                        self.active.insert((*unit).to_owned(), false);
                    }
                    out(0, "")
                }
                ["--user", "enable", unit] => {
                    self.enabled.insert((*unit).to_owned(), true);
                    out(0, "")
                }
                ["--user", "disable", unit] => {
                    self.enabled.insert((*unit).to_owned(), false);
                    out(0, "")
                }
                ["--user", "daemon-reload"] => out(0, ""),
                _ => panic!(
                    "unexpected systemctl invocation in test: {}",
                    args.join(" ")
                ),
            }
        }

        fn answer_launchctl(&mut self, args: &[&str]) -> CommandResult {
            let out = |code: i32, stdout: &str, stderr: &str| CommandResult {
                code: Some(code),
                stdout: stdout.to_owned(),
                stderr: stderr.to_owned(),
            };
            match args {
                ["print", _] => {
                    if self.bootstrapped {
                        let state = if self.job_running {
                            "running"
                        } else {
                            "exited"
                        };
                        let mut stdout = format!("gui/501/app.toka.ts = {{\n\tstate = {state};\n");
                        if let Some(program) = &self.program {
                            stdout.push_str(&format!("\tprogram = {program};\n"));
                        }
                        stdout.push_str("}\n");
                        out(0, &stdout, "")
                    } else {
                        out(1, "", "Could not find service in domain\n")
                    }
                }
                ["bootstrap", _, _] => {
                    self.bootstrapped = true;
                    self.job_running = true;
                    out(0, "", "")
                }
                ["bootout", _] => {
                    if !self.bootout_noop {
                        self.bootstrapped = false;
                        self.job_running = false;
                    }
                    out(0, "", "")
                }
                ["kickstart", "-k", _] => {
                    if self.bootstrapped {
                        self.job_running = true;
                        out(0, "", "")
                    } else {
                        out(1, "", "Could not find service in domain\n")
                    }
                }
                _ => panic!(
                    "unexpected launchctl invocation in test: {}",
                    args.join(" ")
                ),
            }
        }

        fn answer_loginctl(&mut self, args: &[&str]) -> CommandResult {
            let out = |code: i32, stdout: &str| CommandResult {
                code: Some(code),
                stdout: stdout.to_owned(),
                stderr: String::new(),
            };
            match args {
                ["show-user", _, "-p", "Linger", "--value"] => {
                    out(0, if self.lingering { "yes\n" } else { "no\n" })
                }
                ["enable-linger", _] => {
                    self.lingering = true;
                    out(0, "")
                }
                _ => panic!("unexpected loginctl invocation in test: {}", args.join(" ")),
            }
        }
    }

    struct Fake {
        temp: tempfile::TempDir,
        executable: PathBuf,
        temp_dir: PathBuf,
        config: PathBuf,
        uid: u32,
        user: Option<String>,
        calls: Vec<String>,
        script: Rc<RefCell<Script>>,
        purge: Vec<crate::reset::ResetTarget>,
        purge_error: Option<String>,
        lock_held: bool,
        /// The environment the service file pins: scripted per test, since
        /// the real one reads the installing shell.
        env: Vec<(String, String)>,
    }

    impl Fake {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let executable = temp.path().join("bin").join("ts");
            std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
            std::fs::write(&executable, b"ts").unwrap();
            // A scratch temp dir beside the executable, mirroring the CLI
            // harness (TMPDIR under the scratch home, binary elsewhere):
            // installs from the default executable are not ephemeral.
            let temp_dir = temp.path().join("tmp");
            let config = temp.path().join(".config");
            Self {
                temp,
                executable,
                temp_dir,
                config,
                uid: 501,
                user: Some("tester".to_owned()),
                calls: Vec::new(),
                script: Rc::new(RefCell::new(Script::default())),
                purge: Vec::new(),
                purge_error: None,
                lock_held: false,
                env: Vec::new(),
            }
        }

        fn home(&self) -> PathBuf {
            self.temp.path().to_path_buf()
        }

        fn config(&self) -> PathBuf {
            self.config.clone()
        }

        fn changed(&self, outcome: &Outcome) -> Vec<String> {
            outcome
                .actions
                .iter()
                .filter(|action| action.changed)
                .map(|action| action.message.clone())
                .collect()
        }

        fn unchanged(&self, outcome: &Outcome) -> Vec<String> {
            outcome
                .actions
                .iter()
                .filter(|action| !action.changed)
                .map(|action| action.message.clone())
                .collect()
        }

        fn calls_to(&self, verb: &str) -> Vec<&str> {
            self.calls
                .iter()
                .filter(|call| call.contains(verb))
                .map(String::as_str)
                .collect()
        }
    }

    impl Host for Fake {
        fn home(&self) -> PathBuf {
            self.temp.path().to_path_buf()
        }

        fn config_dir(&self) -> PathBuf {
            self.config.clone()
        }

        fn executable(&self) -> PathBuf {
            self.executable.clone()
        }

        fn temp_dir(&self) -> PathBuf {
            self.temp_dir.clone()
        }

        fn uid(&self) -> u32 {
            self.uid
        }

        fn user(&self) -> Option<String> {
            self.user.clone()
        }

        fn run(&mut self, program: &str, args: &[&str]) -> Result<CommandResult, String> {
            self.calls.push(format!("{program} {}", args.join(" ")));
            if program == "systemctl" {
                if let ["--user", "show", "-p", "ExecStart", unit] = args {
                    // The unit file on disk answers, but missing and
                    // injected failures still apply, as in `Script::answer`.
                    if let Some(outcome) = self.scripted_outcome(program, args)? {
                        return Ok(outcome);
                    }
                    return Ok(self.answer_show(unit));
                }
                if let ["--user", "is-active", unit] = args {
                    if let Some(outcome) = self.scripted_outcome(program, args)? {
                        return Ok(outcome);
                    }
                    if let Some(answer) = self.answer_never_loaded(unit) {
                        return Ok(answer);
                    }
                }
            }
            if program == "launchctl" && args.first() == Some(&"bootstrap") {
                let result = self.script.borrow_mut().answer(program, args)?;
                // A current launchctl reports the loaded agent's program:
                // bootstrapping records it, so installs converge. Tests
                // simulating old launchctl clear `program` themselves.
                if result.success() {
                    self.script.borrow_mut().program =
                        Some(self.executable.to_string_lossy().into_owned());
                }
                return Ok(result);
            }
            self.script.borrow_mut().answer(program, args)
        }

        fn purge_paths(&mut self) -> Result<Vec<crate::reset::ResetTarget>, String> {
            if let Some(error) = &self.purge_error {
                return Err(error.clone());
            }
            Ok(self.purge.clone())
        }

        fn service_env(&self) -> Vec<(String, String)> {
            self.env.clone()
        }

        fn daemon_lock_held(&mut self) -> Result<bool, String> {
            Ok(self.lock_held)
        }
    }

    impl Fake {
        /// Missing binaries and injected failures, shared by the
        /// intercepted verbs so they behave exactly as `Script::answer`.
        fn scripted_outcome(
            &self,
            program: &str,
            args: &[&str],
        ) -> Result<Option<CommandResult>, String> {
            let script = self.script.borrow();
            if script.missing.contains(program) {
                return Err(format!("could not run {program}"));
            }
            let call = format!("{program} {}", args.join(" "));
            if script.fail.contains(&call) {
                let stderr = script
                    .fail_stderr
                    .get(&call)
                    .cloned()
                    .unwrap_or_else(|| "injected failure".to_owned());
                return Ok(Some(CommandResult {
                    code: Some(1),
                    stdout: String::new(),
                    stderr,
                }));
            }
            Ok(None)
        }

        /// Real systemctl's answer for a unit it never loaded — exit-4
        /// `inactive` since systemd 258 — when the script pins no manager
        /// state for the unit and no unit file exists for the manager to
        /// load on demand. `None` delegates to the script: pinned state,
        /// or a present file the manager would load, answers there.
        fn answer_never_loaded(&self, unit: &str) -> Option<CommandResult> {
            let script = self.script.borrow();
            if script.active.contains_key(unit)
                || script.active_unknown.contains(unit)
                || script.activating.contains(unit)
                || script.active_word.contains_key(unit)
            {
                return None;
            }
            drop(script);
            if self.unit_file(unit).is_some_and(|file| file.is_file()) {
                return None;
            }
            Some(CommandResult {
                code: Some(4),
                stdout: "inactive\n".to_owned(),
                stderr: String::new(),
            })
        }

        /// The unit file the manager would load for `unit`, for the two
        /// units these tests know.
        fn unit_file(&self, unit: &str) -> Option<PathBuf> {
            if unit == crate::config::TS_SERVICE_NAME {
                Some(unit_path(&self.config))
            } else if unit == crate::config::INDEXER_SERVICE_NAME {
                Some(legacy_unit_path(&self.home()))
            } else {
                None
            }
        }

        /// Synthesizes `systemctl show -p ExecStart` from the unit file on
        /// disk, so takeover tests install from one executable and
        /// reinstall from another without scripting manager state. With
        /// no file the answer is an empty `ExecStart=`, as a never-loaded
        /// unit reports.
        fn answer_show(&self, unit: &str) -> CommandResult {
            let home = self.temp.path().to_path_buf();
            let file = if unit == crate::config::TS_SERVICE_NAME {
                unit_path(&self.config)
            } else if unit == crate::config::INDEXER_SERVICE_NAME {
                legacy_unit_path(&home)
            } else {
                home.join("no-such-unit.service")
            };
            let report = std::fs::read_to_string(&file)
                .ok()
                .and_then(|contents| show_exec_start(&contents));
            Self::show_output(report)
        }

        fn show_output(report: Option<String>) -> CommandResult {
            let stdout = report
                .map(|executable| {
                    format!(
                        "ExecStart={{ path={executable} ; argv[]={executable} daemon run ; ignore_errors=no }}\n"
                    )
                })
                .unwrap_or_else(|| "ExecStart=\n".to_owned());
            CommandResult {
                code: Some(0),
                stdout,
                stderr: String::new(),
            }
        }
    }

    /// The executable path from a unit file's `ExecStart="..."` line,
    /// reversing the quoting [`systemd_escape`] applies.
    fn show_exec_start(contents: &str) -> Option<String> {
        for line in contents.lines() {
            let Some(rest) = line.trim().strip_prefix("ExecStart=") else {
                continue;
            };
            let quoted = rest.trim().strip_prefix('"')?;
            let mut path = String::new();
            let mut chars = quoted.chars();
            while let Some(char) = chars.next() {
                if char == '\\' {
                    path.push(chars.next()?);
                } else if char == '"' {
                    return Some(path.replace("%%", "%"));
                } else {
                    path.push(char);
                }
            }
            return None;
        }
        None
    }

    #[test]
    fn the_unit_runs_ts_daemon_run_with_idle_scheduling() {
        let unit = render_unit(Path::new("/home/tester/.local/bin/ts"), &[]).unwrap();

        assert!(unit.contains("Description=Toka search indexer\n"), "{unit}");
        assert!(
            unit.contains("ExecStart=\"/home/tester/.local/bin/ts\" daemon run\n"),
            "{unit}"
        );
        for line in [
            "Type=exec\n",
            "Restart=on-failure\n",
            "RestartSec=2\n",
            "Nice=10\n",
            "IOSchedulingClass=idle\n",
            "WantedBy=default.target\n",
        ] {
            assert!(unit.contains(line), "{unit}");
        }
    }

    #[test]
    fn the_unit_escapes_backslashes_and_quotes_like_the_installer() {
        let unit = render_unit(Path::new("/weird\\dir/ts\"x"), &[]).unwrap();

        assert!(
            unit.contains("ExecStart=\"/weird\\\\dir/ts\\\"x\" daemon run\n"),
            "{unit}"
        );
    }

    #[test]
    fn the_plist_keeps_a_background_agent_alive_at_login() {
        let plist = render_plist(Path::new("/Users/tester/.local/bin/ts"), &[]);

        assert!(plist.contains("<string>app.toka.ts</string>"), "{plist}");
        assert!(
            plist.contains("<string>/Users/tester/.local/bin/ts</string>\n\t\t<string>daemon</string>\n\t\t<string>run</string>"),
            "{plist}"
        );
        for (key, value) in [
            ("KeepAlive", "<true/>"),
            ("RunAtLoad", "<true/>"),
            ("ProcessType", "<string>Background</string>"),
            ("LowPriorityIO", "<true/>"),
        ] {
            assert!(plist.contains(&format!("<key>{key}</key>")), "{plist}");
            assert!(plist.contains(value), "{plist}");
        }
    }

    #[test]
    fn the_plist_escapes_xml_metacharacters_in_the_path() {
        let plist = render_plist(Path::new("/tmp/a&b/<ts>"), &[]);

        assert!(plist.contains("/tmp/a&amp;b/&lt;ts&gt;"), "{plist}");
    }

    #[test]
    fn systemd_query_without_a_unit_file_runs_no_commands() {
        let mut host = Fake::new();

        let state = query_on(&mut host, ServiceManager::Systemd);

        assert_eq!(
            state,
            ServiceState {
                installed: Some(false),
                enabled: None,
                running: None,
            }
        );
        assert!(host.calls.is_empty());
    }

    #[test]
    fn systemd_query_reports_enabled_and_running() {
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .enabled
            .insert("toka-search.service".into(), true);
        host.script
            .borrow_mut()
            .active
            .insert("toka-search.service".into(), true);

        let state = query_on(&mut host, ServiceManager::Systemd);

        assert_eq!(
            state,
            ServiceState {
                installed: Some(true),
                enabled: Some(true),
                running: Some(true),
            }
        );
    }

    #[test]
    fn systemd_query_reports_disabled_and_stopped() {
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();

        let state = query_on(&mut host, ServiceManager::Systemd);

        assert_eq!(
            state,
            ServiceState {
                installed: Some(true),
                enabled: Some(false),
                running: Some(false),
            }
        );
    }

    #[test]
    fn systemd_query_counts_runtime_enablement_as_enabled() {
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .enabled_runtime
            .insert("toka-search.service".into());

        let state = query_on(&mut host, ServiceManager::Systemd);

        // Status reports the running-today answer; only install insists
        // on the persistent one.
        assert_eq!(
            state,
            ServiceState {
                installed: Some(true),
                enabled: Some(true),
                running: Some(false),
            }
        );
    }

    #[test]
    fn systemd_query_without_systemctl_leaves_state_unknown() {
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
        host.script.borrow_mut().missing.insert("systemctl".into());

        let state = query_on(&mut host, ServiceManager::Systemd);

        assert_eq!(
            state,
            ServiceState {
                installed: Some(true),
                enabled: None,
                running: None,
            }
        );
    }

    #[test]
    fn launchd_query_without_a_plist_runs_no_commands() {
        let mut host = Fake::new();

        let state = query_on(&mut host, ServiceManager::Launchd);

        assert_eq!(
            state,
            ServiceState {
                installed: Some(false),
                enabled: None,
                running: None,
            }
        );
        assert!(host.calls.is_empty());
    }

    #[test]
    fn launchd_query_reports_bootstrapped_and_running() {
        let mut host = Fake::new();
        std::fs::create_dir_all(plist_path(&host.home()).parent().unwrap()).unwrap();
        std::fs::write(plist_path(&host.home()), b"<plist/>").unwrap();
        host.script.borrow_mut().bootstrapped = true;
        host.script.borrow_mut().job_running = true;

        let state = query_on(&mut host, ServiceManager::Launchd);

        assert_eq!(
            state,
            ServiceState {
                installed: Some(true),
                enabled: Some(true),
                running: Some(true),
            }
        );
    }

    #[test]
    fn launchd_query_reports_present_but_not_bootstrapped() {
        let mut host = Fake::new();
        std::fs::create_dir_all(plist_path(&host.home()).parent().unwrap()).unwrap();
        std::fs::write(plist_path(&host.home()), b"<plist/>").unwrap();

        let state = query_on(&mut host, ServiceManager::Launchd);

        // Not bootstrapped, so not enabled; running stays unknown — a
        // hand-started daemon may still run there.
        assert_eq!(
            state,
            ServiceState {
                installed: Some(true),
                enabled: Some(false),
                running: None,
            }
        );
    }

    #[test]
    fn launchd_query_reads_an_exited_job_as_stopped() {
        assert_eq!(launchd_running_state("state = running\n"), Some(true));
        assert_eq!(launchd_running_state("state=running\n"), Some(true));
        assert_eq!(launchd_running_state("\tstate = exited;\n"), Some(false));
        assert_eq!(
            launchd_running_state("gui/501/app.toka.ts = {\n\tstate = spawn scheduled;\n}\n"),
            Some(false)
        );
        assert_eq!(launchd_running_state("no state here\n"), None);
        // The key is `state`, exactly — not a prefix of `statements`.
        assert_eq!(launchd_running_state("statements = running\n"), None);
    }

    #[test]
    fn systemd_install_writes_enables_starts_and_lingers() {
        let mut host = Fake::new();
        let config = host.config();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(unit_path(&config)).unwrap(),
            render_unit(&host.executable, &[]).unwrap()
        );
        assert_eq!(host.changed(&outcome).len(), 5);
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action.starts_with("wrote ~/")),
            "{}",
            host.changed(&outcome).join("\n")
        );
        for verb in ["daemon-reload", "enable", "start", "enable-linger"] {
            assert!(!host.calls_to(verb).is_empty(), "{verb}: {:?}", host.calls);
        }
        // The legacy check ran (and found nothing), but no legacy verb did.
        assert!(!host.calls_to("toka-indexer").is_empty());
        assert!(host.calls_to("stop toka-indexer").is_empty());
        assert_eq!(
            host.unchanged(&outcome),
            vec!["no old toka-indexer.service to migrate".to_owned()]
        );
        let script = host.script.borrow();
        assert_eq!(script.enabled.get("toka-search.service"), Some(&true));
        assert_eq!(script.active.get("toka-search.service"), Some(&true));
        assert!(script.lingering);
    }

    #[test]
    fn systemd_install_is_idempotent() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(host.changed(&outcome).is_empty());
        assert_eq!(host.unchanged(&outcome).len(), 5);
        // Convergence checks only: nothing to enable, start, or reload.
        for verb in ["daemon-reload", "enable toka-search", "start toka-search"] {
            assert!(host.calls_to(verb).is_empty(), "{verb}: {:?}", host.calls);
        }
    }

    #[test]
    fn systemd_install_migrates_the_old_service() {
        let mut host = Fake::new();
        let legacy = legacy_unit_path(&host.home());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active
            .insert("toka-indexer.service".into(), true);
        host.script
            .borrow_mut()
            .enabled
            .insert("toka-indexer.service".into(), true);

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(!legacy.exists());
        for verb in ["stop toka-indexer.service", "disable toka-indexer.service"] {
            assert!(!host.calls_to(verb).is_empty(), "{verb}: {:?}", host.calls);
        }
        // The old service stops before the new one starts.
        let stop = host
            .calls
            .iter()
            .position(|call| call.contains("stop toka-indexer"))
            .unwrap();
        let start = host
            .calls
            .iter()
            .position(|call| call.contains("start toka-search"))
            .unwrap();
        assert!(stop < start);
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "stopped the old toka-indexer.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn systemd_install_fails_when_the_old_service_refuses_to_stop() {
        let mut host = Fake::new();
        let legacy = legacy_unit_path(&host.home());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active
            .insert("toka-indexer.service".into(), true);
        host.script
            .borrow_mut()
            .fail
            .insert("systemctl --user stop toka-indexer.service".to_owned());

        let (error, partial) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();

        assert!(
            error.contains("systemctl --user stop toka-indexer.service"),
            "{error}"
        );
        assert!(legacy.exists(), "a failed migration removes nothing");
        assert!(host.calls_to("start toka-search").is_empty());
        // The failure still reports what completed before it.
        assert!(
            host.changed(&partial)
                .iter()
                .any(|action| action.starts_with("wrote ~/")),
            "{}",
            host.changed(&partial).join("\n")
        );
    }

    #[test]
    fn systemd_install_guides_when_lingering_is_refused() {
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .fail
            .insert("loginctl enable-linger tester".to_owned());

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            host.unchanged(&outcome)
                .iter()
                .any(|action| action.contains("loginctl enable-linger tester")),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
    }

    #[test]
    fn launchd_install_writes_and_bootstraps() {
        let mut host = Fake::new();
        let home = host.home();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(plist_path(&home)).unwrap(),
            render_plist(&host.executable, &[])
        );
        assert_eq!(host.changed(&outcome).len(), 2);
        assert!(!host.calls_to("bootstrap").is_empty());
        let script = host.script.borrow();
        assert!(script.bootstrapped);
        assert!(script.job_running);
    }

    #[test]
    fn launchd_install_is_idempotent() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(host.changed(&outcome).is_empty());
        assert_eq!(
            host.unchanged(&outcome),
            vec![
                "service file already up to date".to_owned(),
                "app.toka.ts is already running".to_owned(),
            ]
        );
        assert!(host.calls_to("bootstrap").is_empty());
        assert!(host.calls_to("bootout").is_empty());
    }

    #[test]
    fn launchd_install_reloads_a_changed_plist() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        // The executable moved: the next install rewrites the plist.
        host.executable = host.home().join("elsewhere").join("ts");
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(!host.calls_to("bootout").is_empty());
        assert!(!host.calls_to("bootstrap").is_empty());
        // The stop records before the load: a bootstrap failure still
        // reports the bootout that already ran.
        let changed = host.changed(&outcome);
        let stopped = changed
            .iter()
            .position(|action| action == "stopped app.toka.ts")
            .unwrap();
        let started = changed
            .iter()
            .position(|action| action == "started app.toka.ts")
            .unwrap();
        assert!(stopped < started, "{}", changed.join("\n"));
    }

    #[test]
    fn launchd_install_kickstarts_a_loaded_but_stopped_agent() {
        let mut host = Fake::new();
        std::fs::create_dir_all(plist_path(&host.home()).parent().unwrap()).unwrap();
        std::fs::write(
            plist_path(&host.home()),
            render_plist(&host.executable, &[]),
        )
        .unwrap();
        host.script.borrow_mut().bootstrapped = true;
        host.script.borrow_mut().job_running = false;
        // Old launchctl, naming no loaded program: the install converges
        // from the plist file plus the marker alone, without consulting
        // the program line.
        host.script.borrow_mut().program = None;

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(!host.calls_to("kickstart").is_empty());
        assert!(host.calls_to("bootstrap").is_empty());
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "started app.toka.ts"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn launchd_install_fails_when_launchctl_is_missing() {
        let mut host = Fake::new();
        host.script.borrow_mut().missing.insert("launchctl".into());

        let (error, partial) = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();

        assert!(error.contains("launchd"), "{error}");
        // The failure still reports what completed before it.
        assert!(
            host.changed(&partial)
                .iter()
                .any(|action| action.starts_with("wrote ~/")),
            "{}",
            host.changed(&partial).join("\n")
        );
    }

    #[test]
    #[cfg(unix)]
    fn install_with_link_links_and_relinks_idempotently() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            let outcome =
                perform_on(&mut host, manager, ServiceOp::Install { link: true }).unwrap();
            assert!(
                host.changed(&outcome)
                    .iter()
                    .any(|action| action == "linked ~/.local/bin/ts"),
                "{manager:?}: {}",
                host.changed(&outcome).join("\n")
            );
            assert_eq!(
                std::fs::read_link(link_path(&host.home())).unwrap(),
                host.executable
            );

            let outcome =
                perform_on(&mut host, manager, ServiceOp::Install { link: true }).unwrap();
            assert!(
                host.unchanged(&outcome)
                    .iter()
                    .any(|action| action == "~/.local/bin/ts is already linked"),
                "{manager:?}: {}",
                host.unchanged(&outcome).join("\n")
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn install_with_link_replaces_a_stale_link() {
        let mut host = Fake::new();
        let link = link_path(&host.home());
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("/elsewhere/ts", &link).unwrap();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: true },
        )
        .unwrap();

        assert_eq!(std::fs::read_link(&link).unwrap(), host.executable);
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "linked ~/.local/bin/ts"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn install_with_link_warns_when_a_directory_is_in_the_way() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            std::fs::create_dir_all(link_path(&host.home())).unwrap();

            // The link is the explicitly optional `--link` part: a blocked
            // link warns while the installed service still exits 0, so the
            // installer restarts onto the new binary instead of skipping it.
            let outcome = perform_on(&mut host, manager, ServiceOp::Install { link: true })
                .unwrap_or_else(|(error, _)| {
                    panic!("{manager:?}: a blocked link must warn, not fail: {error}")
                });

            assert!(
                host.unchanged(&outcome)
                    .iter()
                    .any(|action| action.contains("warning")
                        && action.contains("a directory is in the way")),
                "{manager:?}: {}",
                host.unchanged(&outcome).join("\n")
            );
            // The service installed before the link warned, and the report
            // says so.
            assert!(
                !host.changed(&outcome).is_empty(),
                "{manager:?}: {}",
                host.changed(&outcome).join("\n")
            );
        }
    }

    #[test]
    fn install_refuses_an_executable_under_the_temp_dir() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            std::fs::create_dir_all(&host.temp_dir).unwrap();
            let ephemeral = host.temp_dir.join("ts");
            std::fs::write(&ephemeral, b"ts").unwrap();
            host.executable = ephemeral;
            let home = host.home();
            let config = host.config();

            let (error, partial) =
                perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap_err();

            assert!(
                error.contains("temporary copy") && error.contains("install Toka first"),
                "{manager:?}: {error}"
            );
            assert!(partial.actions.is_empty());
            assert!(!unit_path(&config).exists(), "{manager:?}");
            assert!(!plist_path(&home).exists(), "{manager:?}");
            assert!(host.calls.is_empty(), "{manager:?}: {:?}", host.calls);
        }
    }

    #[test]
    fn install_refuses_an_appimage_mount_executable() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            // A bare AppImage mount: outside the temp dir on purpose, so
            // only the mount pattern refuses it.
            host.executable = PathBuf::from("/tmp/.mount_ABC123/ts");

            let (error, _) =
                perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap_err();

            assert!(error.contains("temporary copy"), "{manager:?}: {error}");
        }
    }

    #[test]
    fn install_refuses_a_deleted_executable() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            host.executable = PathBuf::from("/opt/toka/ts (deleted)");

            let (error, _) =
                perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap_err();

            assert!(error.contains("temporary copy"), "{manager:?}: {error}");
        }
    }

    #[test]
    fn ephemeral_executables_are_temp_mounted_or_deleted() {
        let temp = Path::new("/tmp");
        assert!(is_ephemeral_executable(Path::new("/tmp/ts"), temp));
        assert!(is_ephemeral_executable(
            Path::new("/tmp/.mount_ABC123/usr/bin/ts"),
            Path::new("/var/tmp")
        ));
        assert!(is_ephemeral_executable(
            Path::new("/opt/toka/ts (deleted)"),
            temp
        ));
        assert!(!is_ephemeral_executable(Path::new("/tmpfoo/ts"), temp));
        assert!(!is_ephemeral_executable(Path::new("/usr/bin/ts"), temp));
    }

    #[test]
    #[cfg(unix)]
    fn ephemeral_check_canonicalizes_a_symlinked_temp_dir() {
        let sandbox = tempfile::tempdir().unwrap();
        let real = sandbox.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("ts"), b"fake").unwrap();
        let alias = sandbox.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        // Spelled through the symlink, the executable lexically misses
        // the temp dir — but canonically it sits inside it.
        let spelled = alias.join("ts");
        assert!(!spelled.starts_with(&real));
        assert!(is_ephemeral_executable(&spelled, &real));
    }

    #[test]
    #[cfg(unix)]
    fn install_with_link_still_links_when_the_manager_is_unreachable() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            let program = match manager {
                ServiceManager::Systemd => "systemctl",
                ServiceManager::Launchd => "launchctl",
            };
            host.script.borrow_mut().missing.insert(program.to_owned());

            // The service setup fails, so the install still exits
            // non-zero — but the link rides along when it succeeds.
            let (error, partial) =
                perform_on(&mut host, manager, ServiceOp::Install { link: true }).unwrap_err();

            assert!(error.contains("could not ask"), "{manager:?}: {error}");
            assert_eq!(
                std::fs::read_link(link_path(&host.home())).unwrap(),
                host.executable,
                "{manager:?}"
            );
            assert!(
                host.changed(&partial)
                    .iter()
                    .any(|action| action == "linked ~/.local/bin/ts"),
                "{manager:?}: {}",
                host.changed(&partial).join("\n")
            );
        }
    }

    #[test]
    fn install_reports_the_service_error_when_both_service_and_link_fail() {
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .missing
            .insert("systemctl".to_owned());
        let link = link_path(&host.home());
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::write(&link, b"a real copied binary").unwrap();

        let (error, partial) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: true },
        )
        .unwrap_err();

        // The service error stays primary; the link failure rides along
        // as a warning instead of replacing it.
        assert!(error.contains("could not ask"), "{error}");
        assert!(
            host.unchanged(&partial)
                .iter()
                .any(|action| action.contains("warning")
                    && action.contains("a regular file is in the way")),
            "{}",
            host.unchanged(&partial).join("\n")
        );
    }

    #[test]
    fn systemd_uninstall_stops_disables_and_removes() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert!(!unit_path(&host.config_dir()).exists());
        for verb in ["stop toka-search", "disable toka-search", "daemon-reload"] {
            assert!(!host.calls_to(verb).is_empty(), "{verb}: {:?}", host.calls);
        }
        assert_eq!(host.changed(&outcome).len(), 4);
        assert!(host.unchanged(&outcome).is_empty());
    }

    #[test]
    fn systemd_uninstall_is_idempotent_when_absent() {
        // The never-loaded probes answer exit-4 (systemd 258): confirmed
        // absent, so repeated uninstalls proceed without stopping
        // nonexistent units.
        let mut host = Fake::new();

        for _ in 0..2 {
            let outcome = perform_on(
                &mut host,
                ServiceManager::Systemd,
                ServiceOp::Uninstall { purge_data: false },
            )
            .unwrap();

            assert_eq!(
                host.unchanged(&outcome),
                vec!["toka-search.service is not installed".to_owned()]
            );
            assert!(host.changed(&outcome).is_empty());
        }
        for verb in ["stop toka-search", "disable", "daemon-reload"] {
            assert!(host.calls_to(verb).is_empty(), "{verb}: {:?}", host.calls);
        }
    }

    #[test]
    fn systemd_uninstall_fails_when_the_service_refuses_to_stop() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.script
            .borrow_mut()
            .fail
            .insert("systemctl --user stop toka-search.service".to_owned());

        let (error, partial) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap_err();

        assert!(
            error.contains("systemctl --user stop toka-search.service"),
            "{error}"
        );
        assert!(
            unit_path(&host.config_dir()).exists(),
            "a failed stop removes nothing"
        );
        assert!(
            partial.actions.is_empty(),
            "nothing completed before the stop failed"
        );
    }

    #[test]
    fn launchd_uninstall_bootouts_and_removes() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert!(!plist_path(&host.home()).exists());
        assert!(!host.calls_to("bootout").is_empty());
        assert_eq!(host.changed(&outcome).len(), 2);
    }

    #[test]
    fn launchd_uninstall_is_idempotent_when_absent() {
        let mut host = Fake::new();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert_eq!(
            host.unchanged(&outcome),
            vec!["app.toka.ts is not installed".to_owned()]
        );
        assert!(host.changed(&outcome).is_empty());
        assert!(host.calls_to("bootout").is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn uninstall_removes_only_a_link_pointing_here() {
        for points_here in [true, false] {
            let mut host = Fake::new();
            let link = link_path(&host.home());
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            let target = if points_here {
                host.executable.clone()
            } else {
                PathBuf::from("/elsewhere/ts")
            };
            std::os::unix::fs::symlink(&target, &link).unwrap();

            let outcome = perform_on(
                &mut host,
                ServiceManager::Launchd,
                ServiceOp::Uninstall { purge_data: false },
            )
            .unwrap();

            if points_here {
                assert!(!link.exists() && std::fs::symlink_metadata(&link).is_err());
                assert!(
                    host.changed(&outcome)
                        .iter()
                        .any(|action| action == "removed ~/.local/bin/ts"),
                    "{}",
                    host.changed(&outcome).join("\n")
                );
            } else {
                assert_eq!(std::fs::read_link(&link).unwrap(), target);
                assert!(host.changed(&outcome).is_empty());
            }
        }
    }

    #[test]
    fn uninstall_purge_removes_only_the_listed_data() {
        let mut host = Fake::new();
        let home = host.home();
        let index = home.join("index");
        let logs = home.join("logs");
        let cache = home.join("cache");
        let media = home.join("media");
        std::fs::create_dir_all(&index).unwrap();
        std::fs::write(index.join("index.sqlite3"), b"db").unwrap();
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("search.log"), b"log").unwrap();
        std::fs::write(&cache, b"thumb").unwrap();
        std::fs::create_dir_all(&media).unwrap();
        std::fs::write(media.join("clip.mp4"), b"video").unwrap();
        host.purge = vec![
            ResetTarget::dir(index.clone()),
            ResetTarget::dir(logs.clone()),
            ResetTarget::file(cache.clone()),
        ];

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap();

        assert!(!index.exists());
        assert!(!logs.exists());
        assert!(!cache.exists());
        assert!(media.join("clip.mp4").is_file(), "media is never purged");
        assert_eq!(host.changed(&outcome).len(), 3);
    }

    #[test]
    fn uninstall_purge_with_no_data_left_reports_already() {
        let mut host = Fake::new();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap();

        assert!(
            host.unchanged(&outcome)
                .iter()
                .any(|action| action == "no index data to remove"),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
    }

    #[test]
    #[cfg(unix)]
    fn uninstall_purge_failure_names_what_resisted() {
        // Root can remove anything, so this refusal only exists for the
        // rest of us.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let mut host = Fake::new();
        let locked = host.home().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("index.sqlite3"), b"db").unwrap();
        let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o555);
        std::fs::set_permissions(&locked, permissions).unwrap();
        host.purge = vec![ResetTarget::dir(locked.clone())];

        let (error, partial) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap_err();

        assert!(error.contains("could not remove"), "{error}");
        assert!(locked.join("index.sqlite3").is_file());
        // Nothing was installed, so the partial report is just that.
        assert_eq!(
            host.unchanged(&partial),
            vec!["toka-search.service is not installed".to_owned()]
        );
        // Let the tempdir cleanup remove what the purge could not.
        let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&locked, permissions).unwrap();
    }

    #[test]
    fn systemd_start_stop_and_restart_converge() {
        let mut host = Fake::new();

        let (error, _partial) =
            perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Start).unwrap_err();
        assert!(error.contains("not installed"), "{error}");
        let (error, _partial) =
            perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Restart).unwrap_err();
        assert!(error.contains("not installed"), "{error}");
        // Stopping what was never installed is already-held state, not an
        // error — scripts stop before they remove.
        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Stop).unwrap();
        assert_eq!(
            host.unchanged(&outcome),
            vec!["toka-search.service is not installed, nothing to stop".to_owned()]
        );

        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Stop).unwrap();

        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Start).unwrap();
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "started toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Start).unwrap();
        assert!(host.changed(&outcome).is_empty());

        // Restart while stopped still ends up running.
        perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Stop).unwrap();
        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Restart).unwrap();
        assert_eq!(
            host.unchanged(&outcome),
            vec!["toka-search.service is already stopped".to_owned()]
        );
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "started toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );

        // Restart while running stops first, then starts.
        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Restart).unwrap();
        assert_eq!(
            host.changed(&outcome),
            vec![
                "stopped toka-search.service".to_owned(),
                "started toka-search.service".to_owned(),
            ]
        );
        assert!(host.script.borrow().active["toka-search.service"]);
    }

    #[test]
    fn launchd_start_stop_and_restart_converge() {
        let mut host = Fake::new();

        let (error, _partial) =
            perform_on(&mut host, ServiceManager::Launchd, ServiceOp::Start).unwrap_err();
        assert!(error.contains("not installed"), "{error}");
        let (error, _partial) =
            perform_on(&mut host, ServiceManager::Launchd, ServiceOp::Restart).unwrap_err();
        assert!(error.contains("not installed"), "{error}");
        let outcome = perform_on(&mut host, ServiceManager::Launchd, ServiceOp::Stop).unwrap();
        assert_eq!(
            host.unchanged(&outcome),
            vec!["app.toka.ts is not installed, nothing to stop".to_owned()]
        );

        perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        let outcome = perform_on(&mut host, ServiceManager::Launchd, ServiceOp::Start).unwrap();
        assert!(host.changed(&outcome).is_empty());
        perform_on(&mut host, ServiceManager::Launchd, ServiceOp::Stop).unwrap();

        // Start while merely unloaded bootstraps again.
        let outcome = perform_on(&mut host, ServiceManager::Launchd, ServiceOp::Start).unwrap();
        assert!(!host.calls_to("bootstrap").is_empty());
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "started app.toka.ts"),
            "{}",
            host.changed(&outcome).join("\n")
        );

        // Restart while running unloads first, then loads.
        let outcome = perform_on(&mut host, ServiceManager::Launchd, ServiceOp::Restart).unwrap();
        assert_eq!(
            host.changed(&outcome),
            vec![
                "stopped app.toka.ts".to_owned(),
                "started app.toka.ts".to_owned(),
            ]
        );
        assert!(host.script.borrow().bootstrapped);
        assert!(host.script.borrow().job_running);
    }

    #[test]
    fn stop_for_reset_stops_only_running_services() {
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active
            .insert("toka-search.service".into(), true);

        stop_for_reset(&mut host, ServiceManager::Systemd).unwrap();

        assert_eq!(
            host.calls,
            vec![
                "systemctl --user is-active toka-search.service".to_owned(),
                "systemctl --user stop toka-search.service".to_owned(),
                "systemctl --user is-active toka-indexer.service".to_owned()
            ]
        );

        let mut host = Fake::new();
        host.script.borrow_mut().bootstrapped = true;

        stop_for_reset(&mut host, ServiceManager::Launchd).unwrap();

        assert_eq!(host.calls_to("bootout").len(), 1);
    }

    #[test]
    fn stop_for_reset_ignores_an_unreachable_manager() {
        let mut host = Fake::new();
        host.script.borrow_mut().missing.insert("systemctl".into());
        stop_for_reset(&mut host, ServiceManager::Systemd).unwrap();

        let mut host = Fake::new();
        host.script.borrow_mut().missing.insert("launchctl".into());
        stop_for_reset(&mut host, ServiceManager::Launchd).unwrap();
    }

    #[test]
    fn stop_for_reset_fails_when_a_running_service_refuses_to_stop() {
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active
            .insert("toka-search.service".into(), true);
        host.script
            .borrow_mut()
            .fail
            .insert("systemctl --user stop toka-search.service".to_owned());

        let error = stop_for_reset(&mut host, ServiceManager::Systemd).unwrap_err();

        assert!(
            error.contains("systemctl --user stop toka-search.service"),
            "{error}"
        );
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

    #[test]
    fn liveness_maps_the_is_active_vocabulary() {
        use Liveness::{Active, Deactivating, Stopped, Unknown};
        assert_eq!(systemd_liveness_state(Some(0), "active\n", true), Active);
        assert_eq!(systemd_liveness_state(Some(0), "", true), Active);
        assert_eq!(systemd_liveness_state(Some(3), "inactive\n", true), Stopped);
        assert_eq!(systemd_liveness_state(Some(3), "failed\n", true), Stopped);
        // `activating` exits 3 yet must still be stopped: a pending start
        // is cancelled by a stop, never skipped over — and it ends
        // running, so it reads as active.
        assert_eq!(
            systemd_liveness_state(Some(3), "activating\n", true),
            Active
        );
        // `deactivating` splits out: actionable for stop, never skippable
        // for start.
        assert_eq!(
            systemd_liveness_state(Some(3), "deactivating\n", true),
            Deactivating
        );
        assert_eq!(systemd_liveness_state(Some(3), "reloading\n", true), Active);
        assert_eq!(
            systemd_liveness_state(Some(3), "maintenance\n", true),
            Unknown
        );
        assert_eq!(systemd_liveness_state(Some(3), "", true), Unknown);
        assert_eq!(systemd_liveness_state(Some(3), "bogus\n", true), Unknown);
        assert_eq!(systemd_liveness_state(Some(1), "", true), Unknown);
        assert_eq!(systemd_liveness_state(None, "", true), Unknown);
        // systemd 258's confirmed-absent answer: exit 4, with the same
        // `inactive` word — stopped with or without a file on disk, since
        // a unit the manager never loaded has nothing to stop.
        for file_present in [true, false] {
            assert_eq!(
                systemd_liveness_state(Some(4), "inactive\n", file_present),
                Stopped,
                "file_present={file_present}"
            );
            assert_eq!(
                systemd_liveness_state(Some(4), "", file_present),
                Stopped,
                "file_present={file_present}"
            );
        }
        // A `not-found` word names the absence outright: stopped even with
        // a file present — the file simply is not loaded.
        assert_eq!(
            systemd_liveness_state(Some(3), "not-found\n", true),
            Stopped
        );
        assert_eq!(
            systemd_liveness_state(Some(3), "not-found\n", false),
            Stopped
        );
        // Older systemctl's never-loaded answer: with no unit file nothing
        // can run, so it reads as stopped; with a file present it keeps
        // failing closed. A localized word is not the `unknown` word, so
        // it always fails closed.
        assert_eq!(systemd_liveness_state(Some(3), "unknown\n", false), Stopped);
        assert_eq!(systemd_liveness_state(Some(3), "unknown\n", true), Unknown);
        assert_eq!(systemd_liveness_state(Some(3), "inaktiv\n", false), Unknown);
        assert_eq!(systemd_liveness_state(Some(3), "inaktiv\n", true), Unknown);
    }

    #[test]
    fn enablement_maps_the_systemctl_vocabulary() {
        assert_eq!(systemd_enablement_state("enabled\n"), Enablement::Enabled);
        assert_eq!(
            systemd_enablement_state("enabled-runtime\n"),
            Enablement::EnabledRuntime
        );
        for word in [
            "disabled",
            "static",
            "masked",
            "masked-runtime",
            "indirect",
            "generated",
            "bad",
            "alias",
            "not-found",
        ] {
            assert_eq!(
                systemd_enablement_state(&format!("{word}\n")),
                Enablement::Disabled,
                "{word}"
            );
        }
        for garbage in ["", "\n", "   \n", "bogus\n", "linked\n", "linked-runtime\n"] {
            assert_eq!(
                systemd_enablement_state(garbage),
                Enablement::Unknown,
                "{garbage:?}"
            );
        }
    }

    #[test]
    fn launchd_print_maps_failures_to_absent_or_unknown() {
        let loaded = CommandResult {
            code: Some(0),
            stdout: "gui/501/app.toka.ts = {\n\tstate = running;\n\tprogram = /bin/ts;\n}\n"
                .to_owned(),
            stderr: String::new(),
        };
        let state = parse_launchd_print(&loaded).unwrap();
        assert!(state.bootstrapped);
        assert_eq!(state.running, Some(true));

        // The `program` line is not read at all: program-less output from
        // old launchctl parses identically, since staleness is the
        // marker's job.
        let program_less = CommandResult {
            code: Some(0),
            stdout: "gui/501/app.toka.ts = {\n\tstate = running;\n}\n".to_owned(),
            stderr: String::new(),
        };
        let state = parse_launchd_print(&program_less).unwrap();
        assert!(state.bootstrapped);
        assert_eq!(state.running, Some(true));

        let missing = CommandResult {
            code: Some(1),
            stdout: String::new(),
            stderr: "Could not find service gui/501/app.toka.ts in domain".to_owned(),
        };
        let state = parse_launchd_print(&missing).unwrap();
        assert!(!state.bootstrapped);

        // A signal and an unexpected complaint both read as unknown,
        // never as safe-to-delete.
        let signal = CommandResult {
            code: None,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert!(parse_launchd_print(&signal).is_none());
        let other = CommandResult {
            code: Some(1),
            stdout: String::new(),
            stderr: "IPC failure".to_owned(),
        };
        assert!(parse_launchd_print(&other).is_none());
    }

    #[test]
    fn the_unit_doubles_percent_signs_and_rejects_newlines() {
        let unit = render_unit(Path::new("/tmp/100%/ts"), &[]).unwrap();
        assert!(
            unit.contains("ExecStart=\"/tmp/100%%/ts\" daemon run\n"),
            "{unit}"
        );

        let error = render_unit(Path::new("/tmp/a\nb/ts"), &[]).unwrap_err();
        assert!(error.contains("newline"), "{error}");
    }

    #[test]
    fn the_plist_carries_an_explicit_path_for_launchd() {
        let plist = render_plist(Path::new("/Users/tester/.local/bin/ts"), &[]);

        assert!(plist.contains("<key>EnvironmentVariables</key>"), "{plist}");
        assert!(
            plist.contains("<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin</string>"),
            "{plist}"
        );
    }

    #[test]
    fn plist_executable_round_trips_through_rendering() {
        let plist = render_plist(Path::new("/tmp/a&b/<ts>"), &[]);

        assert_eq!(
            parse_plist_executable(&plist),
            Some(PathBuf::from("/tmp/a&b/<ts>"))
        );
        assert_eq!(parse_plist_executable("<plist/>"), None);
        assert_eq!(parse_plist_executable(""), None);
    }

    #[test]
    fn unknown_service_state_reports_installed_unknown() {
        assert_eq!(ServiceState::unknown().installed, None);
    }

    #[test]
    fn for_query_does_not_resolve_the_executable() {
        let host = SystemHost::for_query().unwrap();

        assert!(host.executable().as_os_str().is_empty());
        assert_eq!(host.home(), dirs::home_dir().unwrap());
    }

    #[test]
    fn systemd_stop_fails_closed_when_the_state_is_unknown() {
        // Installed but unreadable: an error, mirroring launchd — never a
        // blind "already stopped".
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-search.service".into());

        let (error, _) =
            perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Stop).unwrap_err();

        assert!(error.contains("could not ask systemd"), "{error}");

        // Nothing installed and nothing to say: already-held state.
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-search.service".into());

        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Stop).unwrap();

        assert_eq!(
            host.unchanged(&outcome),
            vec!["toka-search.service is not installed, nothing to stop".to_owned()]
        );
    }

    #[test]
    fn systemd_stop_stops_a_fileless_but_running_daemon() {
        // The unit file was deleted out of band while the daemon runs:
        // `ts stop` still stops the live daemon instead of reporting
        // "not installed, nothing to stop".
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active
            .insert("toka-search.service".into(), true);

        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Stop).unwrap();

        assert!(
            !host.calls_to("stop toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert_eq!(
            host.changed(&outcome),
            vec!["stopped toka-search.service".to_owned()]
        );
    }

    #[test]
    fn systemd_uninstall_stops_an_unknown_state_daemon_first() {
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-search.service".into());

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert!(
            !host.calls_to("stop toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(!unit_path(&host.config_dir()).exists());
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "stopped toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn uninstall_stop_failure_aborts_before_disable_remove_and_purge() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        let data = host.home().join("data");
        std::fs::write(&data, b"index").unwrap();
        host.purge = vec![ResetTarget::file(data.clone())];
        host.script
            .borrow_mut()
            .fail
            .insert("systemctl --user stop toka-search.service".to_owned());

        let (error, _) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap_err();

        assert!(
            error.contains("systemctl --user stop toka-search.service"),
            "{error}"
        );
        assert!(host.calls_to("disable").is_empty(), "{:?}", host.calls);
        assert!(
            unit_path(&host.config_dir()).exists(),
            "a failed stop removes nothing"
        );
        assert!(data.is_file(), "a failed stop purges nothing");
    }

    #[test]
    fn systemd_restart_errors_when_the_state_is_unknown() {
        // Starting without stopping would silently no-op while a stale
        // daemon runs, so an unreadable state errors like launchd's.
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-search.service".into());

        let (error, _) =
            perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Restart).unwrap_err();

        assert!(error.contains("could not ask systemd"), "{error}");
        assert!(
            host.calls_to("start toka-search.service").is_empty(),
            "no blind start: {:?}",
            host.calls
        );
    }

    #[test]
    fn systemd_start_errors_when_the_state_is_unknown() {
        // Starting blind would report `started` even over an
        // already-running daemon, so an unreadable state errors like
        // restart's instead.
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-search.service".into());

        let (error, _) =
            perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Start).unwrap_err();

        assert!(error.contains("could not ask systemd"), "{error}");
        assert!(
            host.calls_to("start toka-search.service").is_empty(),
            "no blind start: {:?}",
            host.calls
        );
    }

    #[test]
    fn uninstall_disables_an_active_but_fileless_unit() {
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active
            .insert("toka-search.service".into(), true);
        // Dangling enablement for the fileless unit: `is-enabled`
        // confirms it, so the uninstall disables it.
        host.script
            .borrow_mut()
            .enabled
            .insert("toka-search.service".into(), true);

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert!(
            !host.calls_to("disable toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert_eq!(
            host.changed(&outcome),
            vec![
                "stopped toka-search.service".to_owned(),
                "disabled toka-search.service".to_owned(),
            ]
        );
    }

    #[test]
    fn uninstall_skips_disable_without_confirmed_enablement_in_any_locale() {
        // No `is-enabled` confirmation, so no disable is attempted at all:
        // a non-English "not found" can never surface as a hard error.
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active
            .insert("toka-search.service".into(), true);
        host.script
            .borrow_mut()
            .fail
            .insert("systemctl --user disable toka-search.service".to_owned());
        host.script.borrow_mut().fail_stderr.insert(
            "systemctl --user disable toka-search.service".to_owned(),
            "Fehler beim Deaktivieren: Unit-Datei toka-search.service existiert nicht.".to_owned(),
        );

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert!(
            host.calls_to("disable toka-search.service").is_empty(),
            "no blind disable: {:?}",
            host.calls
        );
        assert!(
            host.unchanged(&outcome)
                .iter()
                .any(|action| action == "toka-search.service is not enabled"),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
    }

    #[test]
    fn uninstall_propagates_other_disable_failures_for_a_fileless_unit() {
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active
            .insert("toka-search.service".into(), true);
        host.script
            .borrow_mut()
            .enabled
            .insert("toka-search.service".into(), true);
        host.script
            .borrow_mut()
            .fail
            .insert("systemctl --user disable toka-search.service".to_owned());

        let (error, _) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap_err();

        assert!(error.contains("injected failure"), "{error}");
    }

    #[test]
    fn uninstall_purge_disables_a_stopped_but_enabled_fileless_unit() {
        // A unit the manager loads from outside the config dir reads as
        // fileless here; confirmed stopped, it still must be disabled —
        // deciding the disable from the stop would leave it restarting at
        // the next login.
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .enabled
            .insert("toka-search.service".into(), true);

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap();

        assert!(
            !host.calls_to("disable toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "disabled toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(!host.script.borrow().enabled["toka-search.service"]);
    }

    #[test]
    fn an_activating_daemon_is_stopped_not_skipped() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.script
            .borrow_mut()
            .activating
            .insert("toka-search.service".into());
        host.calls.clear();

        // Restart cancels the pending start before starting.
        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Restart).unwrap();

        assert_eq!(
            host.changed(&outcome),
            vec![
                "stopped toka-search.service".to_owned(),
                "started toka-search.service".to_owned(),
            ]
        );

        // Uninstall stops it too, instead of removing the unit under a
        // daemon that is coming up.
        host.script
            .borrow_mut()
            .activating
            .insert("toka-search.service".into());
        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "stopped toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn systemd_install_restarts_a_rewritten_unit() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        // The executable moved: the next install rewrites the unit.
        host.executable = host.home().join("elsewhere").join("ts");
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        // A reload alone would leave the stale binary running.
        assert!(!host.calls_to("stop toka-search.service").is_empty());
        assert!(!host.calls_to("start toka-search.service").is_empty());
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "restarted toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(
            host.unchanged(&outcome)
                .iter()
                .all(|action| !action.contains("already running")),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
    }

    #[test]
    fn install_from_another_copy_reports_the_takeover() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();
            host.executable = host.home().join("elsewhere").join("ts");

            let outcome =
                perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();

            let label = match manager {
                ServiceManager::Systemd => "toka-search.service",
                ServiceManager::Launchd => "app.toka.ts",
            };
            assert!(
                host.changed(&outcome)
                    .iter()
                    .any(|action| { action.starts_with(&format!("taking over {label} from")) }),
                "{manager:?}: {}",
                host.changed(&outcome).join("\n")
            );
        }
    }

    #[test]
    fn reinstall_of_the_same_executable_reports_no_takeover() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            for _ in 0..2 {
                let outcome =
                    perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();
                assert!(
                    outcome
                        .actions
                        .iter()
                        .all(|action| !action.message.contains("taking over")),
                    "{manager:?}: {}",
                    host.changed(&outcome).join("\n")
                );
            }
        }
    }

    #[test]
    fn install_retry_completes_a_failed_reload() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.executable = host.home().join("elsewhere").join("ts");
        host.script
            .borrow_mut()
            .fail
            .insert("systemctl --user daemon-reload".to_owned());

        // The reload fails after the rewrite: the manager still runs the
        // old binary, and the marker stays set.
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();
        let marker = marker_path(&unit_path(&host.config_dir()));
        assert!(marker.is_file(), "the failed reload leaves its marker");
        host.script.borrow_mut().fail.clear();
        host.calls.clear();

        // The retry reloads and restarts instead of reporting the stale
        // daemon "already running" — driven by the marker, not by
        // comparing executables.
        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            !host.calls_to("daemon-reload").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "restarted toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(!marker.exists(), "the verified reload clears it");
    }

    #[test]
    fn install_enables_persistently_when_only_runtime_enabled() {
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .enabled_runtime
            .insert("toka-search.service".into());

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        // A runtime-only symlink would not survive reboot.
        assert!(
            !host.calls_to("enable toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "enabled toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn launchd_install_retry_completes_a_failed_reload() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.executable = host.home().join("elsewhere").join("ts");
        host.script
            .borrow_mut()
            .fail
            .insert(format!("launchctl bootout {}", launchd_target(&host)));

        // The bootout fails after the rewrite: the loaded agent still
        // runs the old binary, and the marker stays set.
        let (error, partial) = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();
        assert!(error.contains("bootout"), "{error}");
        // The failed bootout records nothing: only a successful stop is
        // reported.
        assert!(
            host.changed(&partial)
                .iter()
                .all(|action| action != "stopped app.toka.ts"),
            "{}",
            host.changed(&partial).join("\n")
        );
        let marker = marker_path(&plist_path(&host.home()));
        assert!(marker.is_file(), "the failed reload leaves its marker");
        host.script.borrow_mut().fail.clear();
        host.calls.clear();

        // The retry reloads instead of reporting the stale agent
        // "already running" — driven by the marker, not by the loaded
        // program.
        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(!host.calls_to("bootout").is_empty(), "{:?}", host.calls);
        assert!(!host.calls_to("bootstrap").is_empty(), "{:?}", host.calls);
        let changed = host.changed(&outcome);
        assert!(
            changed.iter().any(|action| action == "stopped app.toka.ts"),
            "{}",
            changed.join("\n")
        );
        assert!(
            changed.iter().any(|action| action == "started app.toka.ts"),
            "{}",
            changed.join("\n")
        );
        assert!(!marker.exists(), "the verified reload clears it");
    }

    #[test]
    fn uninstall_failing_at_purge_reports_what_it_did() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.purge_error = Some("cannot list index data".to_owned());

        let (error, partial) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap_err();

        assert!(error.contains("cannot list index data"), "{error}");
        for action in [
            "stopped toka-search.service",
            "disabled toka-search.service",
        ] {
            assert!(
                host.changed(&partial).iter().any(|done| done == action),
                "{}",
                host.changed(&partial).join("\n")
            );
        }
        assert!(
            host.changed(&partial)
                .iter()
                .any(|done| done.starts_with("removed ~/")),
            "{}",
            host.changed(&partial).join("\n")
        );
    }

    #[test]
    fn install_failing_at_start_reports_the_migration() {
        let mut host = Fake::new();
        let legacy = legacy_unit_path(&host.home());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active
            .insert("toka-indexer.service".into(), true);
        host.script
            .borrow_mut()
            .fail
            .insert("systemctl --user start toka-search.service".to_owned());

        let (error, partial) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();

        assert!(
            error.contains("systemctl --user start toka-search.service"),
            "{error}"
        );
        for action in [
            "stopped the old toka-indexer.service",
            "disabled the old toka-indexer.service",
        ] {
            assert!(
                host.changed(&partial).iter().any(|done| done == action),
                "{}",
                host.changed(&partial).join("\n")
            );
        }
    }

    #[test]
    fn install_migrates_an_active_but_fileless_legacy_service() {
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active
            .insert("toka-indexer.service".into(), true);

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            !host.calls_to("stop toka-indexer.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.calls_to("disable toka-indexer").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "stopped the old toka-indexer.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn install_disables_a_fileless_legacy_service_left_enabled() {
        // Fileless but running with dangling enablement: the migration
        // stops the legacy daemon and drops the enablement, as an
        // uninstall does.
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active
            .insert("toka-indexer.service".into(), true);
        host.script
            .borrow_mut()
            .enabled
            .insert("toka-indexer.service".into(), true);

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            !host.calls_to("stop toka-indexer.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            !host.calls_to("disable toka-indexer.service").is_empty(),
            "{:?}",
            host.calls
        );
        for message in [
            "stopped the old toka-indexer.service",
            "disabled the old toka-indexer.service",
        ] {
            assert!(
                host.changed(&outcome)
                    .iter()
                    .any(|action| action == message),
                "{}",
                host.changed(&outcome).join("\n")
            );
        }
    }

    #[test]
    fn install_refuses_a_legacy_daemon_it_cannot_probe() {
        // An unreadable legacy probe is indeterminate even with the legacy
        // file present: stopping blind could fail the install, and skipping
        // could strand a live legacy daemon — so the install refuses after
        // one retry, removing nothing.
        let mut host = Fake::new();
        let legacy = legacy_unit_path(&host.home());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-indexer.service".into());

        let (error, _) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();

        assert!(error.contains("refusing to install"), "{error}");
        assert_eq!(
            host.calls_to("is-active toka-indexer.service").len(),
            2,
            "one probe plus one retry: {:?}",
            host.calls
        );
        assert!(
            host.calls_to("stop toka-indexer").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(legacy.exists(), "a refused migration removes nothing");
        assert!(
            host.calls_to("start toka-search").is_empty(),
            "{:?}",
            host.calls
        );
    }

    #[test]
    fn uninstall_purge_refuses_while_the_daemon_holds_the_lock() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();
            let data = host.home().join("data");
            std::fs::write(&data, b"index").unwrap();
            host.purge = vec![ResetTarget::file(data.clone())];
            host.lock_held = true;

            let (error, _) = perform_on(
                &mut host,
                manager,
                ServiceOp::Uninstall { purge_data: true },
            )
            .unwrap_err();

            assert!(error.contains("refusing to purge"), "{error}");
            let service_file = match manager {
                ServiceManager::Systemd => unit_path(&host.config_dir()),
                ServiceManager::Launchd => plist_path(&host.home()),
            };
            assert!(service_file.exists(), "{manager:?}: nothing is removed");
            assert!(data.is_file(), "{manager:?}: nothing is purged");
        }
    }

    #[test]
    fn stop_for_reset_refuses_while_the_daemon_holds_the_lock() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            host.lock_held = true;

            let error = stop_for_reset(&mut host, manager).unwrap_err();

            assert!(error.contains("refusing to reset"), "{error}");
        }
    }

    #[test]
    fn install_with_link_warns_when_a_regular_file_is_in_the_way() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            let link = link_path(&host.home());
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::fs::write(&link, b"a real copied binary").unwrap();

            // The link is the explicitly optional `--link` part: a blocked
            // link warns while the installed service still exits 0, so the
            // installer restarts onto the new binary instead of skipping it.
            let outcome = perform_on(&mut host, manager, ServiceOp::Install { link: true })
                .unwrap_or_else(|(error, _)| {
                    panic!("{manager:?}: a blocked link must warn, not fail: {error}")
                });

            assert!(
                host.unchanged(&outcome)
                    .iter()
                    .any(|action| action.contains("warning")
                        && action.contains("a regular file is in the way")),
                "{manager:?}: {}",
                host.unchanged(&outcome).join("\n")
            );
            assert_eq!(
                std::fs::read(&link).unwrap(),
                b"a real copied binary",
                "{manager:?}: the file survives"
            );
        }
    }

    #[test]
    fn install_with_link_leaves_the_binary_itself_in_place() {
        // The destination already IS this executable: the same path.
        let mut host = Fake::new();
        let link = link_path(&host.home());
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::write(&link, b"ts").unwrap();
        host.executable = link.clone();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: true },
        )
        .unwrap();

        assert!(
            host.unchanged(&outcome)
                .iter()
                .any(|action| action == "~/.local/bin/ts is already the ts binary"),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
        assert_eq!(std::fs::read(&link).unwrap(), b"ts");
        assert!(
            !std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "no path-to-itself symlink is created"
        );
    }

    #[test]
    #[cfg(unix)]
    fn install_with_link_leaves_a_hard_linked_binary_in_place() {
        let mut host = Fake::new();
        let link = link_path(&host.home());
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::hard_link(&host.executable, &link).unwrap();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: true },
        )
        .unwrap();

        assert!(
            host.unchanged(&outcome)
                .iter()
                .any(|action| action == "~/.local/bin/ts is already the ts binary"),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
        assert!(!std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    #[cfg(unix)]
    fn install_replaces_a_symlinked_service_file_itself() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            let file = match manager {
                ServiceManager::Systemd => unit_path(&host.config_dir()),
                ServiceManager::Launchd => plist_path(&host.home()),
            };
            let target = host.home().join("dotfiles-unit");
            std::fs::write(&target, b"[stale]").unwrap();
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&target, &file).unwrap();

            perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();

            // The link itself is replaced — never written through.
            assert!(
                !std::fs::symlink_metadata(&file)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{manager:?}"
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"[stale]", "{manager:?}");
            let expected = match manager {
                ServiceManager::Systemd => render_unit(&host.executable, &[]).unwrap(),
                ServiceManager::Launchd => render_plist(&host.executable, &[]),
            };
            assert_eq!(
                std::fs::read_to_string(&file).unwrap(),
                expected,
                "{manager:?}"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn install_leaves_an_identical_symlinked_service_file() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            let file = match manager {
                ServiceManager::Systemd => unit_path(&host.config_dir()),
                ServiceManager::Launchd => plist_path(&host.home()),
            };
            let rendered = match manager {
                ServiceManager::Systemd => render_unit(&host.executable, &[]).unwrap(),
                ServiceManager::Launchd => render_plist(&host.executable, &[]),
            };
            let target = host.home().join("dotfiles-unit");
            std::fs::write(&target, &rendered).unwrap();
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&target, &file).unwrap();

            let outcome =
                perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();

            assert!(
                host.unchanged(&outcome)
                    .iter()
                    .any(|action| action == "service file already up to date"),
                "{manager:?}: {}",
                host.unchanged(&outcome).join("\n")
            );
            assert!(
                std::fs::symlink_metadata(&file)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{manager:?}: the link survives"
            );
        }
    }

    #[test]
    fn stop_for_reset_refuses_when_installed_but_unreachable() {
        for file in [true, false] {
            let mut host = Fake::new();
            // The ts unit or the legacy one: either installed file refuses.
            let path = if file {
                unit_path(&host.config_dir())
            } else {
                legacy_unit_path(&host.home())
            };
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"[Unit]").unwrap();
            host.script.borrow_mut().missing.insert("systemctl".into());

            let error = stop_for_reset(&mut host, ServiceManager::Systemd).unwrap_err();

            assert!(error.contains("refusing to reset"), "{error}");
        }

        let mut host = Fake::new();
        std::fs::create_dir_all(plist_path(&host.home()).parent().unwrap()).unwrap();
        std::fs::write(plist_path(&host.home()), b"<plist/>").unwrap();
        host.script.borrow_mut().missing.insert("launchctl".into());

        let error = stop_for_reset(&mut host, ServiceManager::Launchd).unwrap_err();

        assert!(error.contains("refusing to reset"), "{error}");
    }

    #[test]
    fn launchd_query_reports_unknown_when_print_fails_unexpectedly() {
        let mut host = Fake::new();
        std::fs::create_dir_all(plist_path(&host.home()).parent().unwrap()).unwrap();
        std::fs::write(plist_path(&host.home()), b"<plist/>").unwrap();
        host.script
            .borrow_mut()
            .fail
            .insert("launchctl print gui/501/app.toka.ts".to_owned());

        let state = query_on(&mut host, ServiceManager::Launchd);

        // An unexpected print failure is unknown, never absent.
        assert_eq!(
            state,
            ServiceState {
                installed: Some(true),
                enabled: None,
                running: None,
            }
        );
    }

    #[test]
    fn install_respects_the_config_directory() {
        let mut host = Fake::new();
        host.config = host.home().join("xdg-config");
        // The legacy unit still migrates from the old installer's
        // hardcoded `~/.config`.
        let legacy = legacy_unit_path(&host.home());
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active
            .insert("toka-indexer.service".into(), true);

        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        let unit = unit_path(&host.config);
        assert!(unit.is_file(), "the unit lands under the config dir");
        assert!(!legacy.exists(), "the legacy unit still migrates away");
        assert!(
            !unit_path(&host.home().join(".config")).exists(),
            "no unit lands under ~/.config"
        );
    }

    #[test]
    fn service_file_writes_are_atomic_and_leave_no_staging_files() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("unit.service");

        assert!(write_service_file(&file, "one").unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one");
        assert!(write_service_file(&file, "two").unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "two");
        assert!(!write_service_file(&file, "two").unwrap());

        let leftovers: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("unit.service")]);
    }

    #[test]
    fn a_failed_service_file_write_keeps_the_previous_config() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("unit.service");
        std::fs::write(&file, "old").unwrap();
        // Block the staging sibling with a directory: the write fails and
        // the previous content survives.
        std::fs::create_dir(sibling_temp_file(&file)).unwrap();

        let error = write_service_file(&file, "new").unwrap_err();

        assert!(error.contains("could not write"), "{error}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old");
    }

    #[test]
    fn never_loaded_units_answer_absent_with_an_empty_show() {
        // The Fake speaks real systemctl for units it never loaded —
        // exit-4 `inactive` since systemd 258 — so the parsers see the
        // same words production does.
        let mut host = Fake::new();

        let probe = host
            .run("systemctl", &["--user", "is-active", "toka-search.service"])
            .unwrap();
        assert_eq!(probe.code, Some(4));
        assert_eq!(probe.stdout, "inactive\n");

        let show = host
            .run(
                "systemctl",
                &["--user", "show", "-p", "ExecStart", "toka-search.service"],
            )
            .unwrap();
        assert_eq!(show.stdout, "ExecStart=\n");

        // A unit file the manager would load on demand still reads as a
        // loaded-but-stopped unit.
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
        let probe = host
            .run("systemctl", &["--user", "is-active", "toka-search.service"])
            .unwrap();
        assert_eq!(probe.stdout, "inactive\n");
    }

    #[test]
    fn fresh_install_reports_started_with_no_bogus_stops() {
        // A fresh machine's manager never loaded either unit — the probes
        // answer exit-4 (systemd 258) — so the install starts the new
        // service (not a restart of nothing) and migrates no old service,
        // without stopping units that do not exist.
        let mut host = Fake::new();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "started toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(
            host.changed(&outcome)
                .iter()
                .all(|action| !action.contains("restarted")),
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(
            host.calls_to("stop").is_empty(),
            "no stops on a fresh machine: {:?}",
            host.calls
        );
        assert!(
            host.unchanged(&outcome)
                .iter()
                .any(|action| action == "no old toka-indexer.service to migrate"),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
    }

    #[test]
    fn fresh_purge_uninstall_prints_zero_bogus_stops() {
        let mut host = Fake::new();
        let data = host.home().join("data");
        std::fs::write(&data, b"index").unwrap();
        host.purge = vec![ResetTarget::file(data.clone())];

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap();

        assert!(
            outcome
                .actions
                .iter()
                .all(|action| !action.message.contains("stopped")),
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(
            host.calls_to("stop").is_empty(),
            "no stops on a fresh machine: {:?}",
            host.calls
        );
        assert!(!data.exists(), "the purge itself still runs");
    }

    #[test]
    fn an_unknown_word_with_a_file_fails_closed() {
        // Exit-3 `unknown` with a unit file installed: a lingering
        // service may still run there, so stop and restart error, and
        // uninstall stops first.
        for op in [ServiceOp::Stop, ServiceOp::Restart] {
            let mut host = Fake::new();
            std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
            std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
            host.script
                .borrow_mut()
                .active_word
                .insert("toka-search.service".into(), "unknown".into());

            let (error, _) = perform_on(&mut host, ServiceManager::Systemd, op).unwrap_err();

            assert!(error.contains("could not ask systemd"), "{op:?}: {error}");
        }

        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active_word
            .insert("toka-search.service".into(), "unknown".into());

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert!(
            !host.calls_to("stop toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(!unit_path(&host.config_dir()).exists());
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "stopped toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn a_localized_word_always_fails_closed() {
        // `is-active` in another language is not the `unknown` word, so an
        // installed file fails closed — while with no file the uninstall
        // still proceeds, since there is nothing to remove.
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.config_dir()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.config_dir()), b"[Unit]").unwrap();
        host.script
            .borrow_mut()
            .active_word
            .insert("toka-search.service".into(), "inaktiv".into());

        let (error, _) =
            perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Stop).unwrap_err();
        assert!(error.contains("could not ask systemd"), "{error}");

        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active_word
            .insert("toka-search.service".into(), "inaktiv".into());

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();
        assert_eq!(
            host.unchanged(&outcome),
            vec!["toka-search.service is not installed".to_owned()]
        );
        assert!(
            host.calls_to("stop toka-search").is_empty(),
            "{:?}",
            host.calls
        );
    }

    #[test]
    fn fileless_unknown_state_proceeds_without_stopping() {
        // A signal-killed probe with nothing installed: the reset's
        // pre-stop and the uninstall both proceed — there is no file to
        // remove and no confirmed daemon to stop.
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-search.service".into());
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-indexer.service".into());

        stop_for_reset(&mut host, ServiceManager::Systemd).unwrap();
        assert!(
            host.calls_to("stop").is_empty(),
            "reset stops nothing fileless: {:?}",
            host.calls
        );

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert_eq!(
            host.unchanged(&outcome),
            vec!["toka-search.service is not installed".to_owned()]
        );
        assert!(
            host.calls_to("stop").is_empty(),
            "uninstall stops nothing fileless: {:?}",
            host.calls
        );

        // Launchd's matching arm: an unexpected print failure with no
        // plist installed proceeds the same way.
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .fail
            .insert("launchctl print gui/501/app.toka.ts".to_owned());

        stop_for_reset(&mut host, ServiceManager::Launchd).unwrap();
        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();
        assert_eq!(
            host.unchanged(&outcome),
            vec!["app.toka.ts is not installed".to_owned()]
        );
    }

    #[test]
    fn fileless_uninstall_proceeds_when_the_manager_is_unreachable() {
        // No service file and no manager to ask: both managers proceed —
        // there is nothing installed to remove — instead of failing on a
        // stop.
        let mut host = Fake::new();
        host.script.borrow_mut().missing.insert("systemctl".into());
        let data = host.home().join("data");
        std::fs::write(&data, b"index").unwrap();
        host.purge = vec![ResetTarget::file(data.clone())];

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap();

        assert_eq!(
            host.unchanged(&outcome),
            vec!["toka-search.service is not installed".to_owned()]
        );
        assert!(!data.exists(), "the purge itself still runs");

        let mut host = Fake::new();
        host.script.borrow_mut().missing.insert("launchctl".into());

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert_eq!(
            host.unchanged(&outcome),
            vec!["app.toka.ts is not installed".to_owned()]
        );
        assert!(host.changed(&outcome).is_empty());
    }

    #[test]
    fn install_migration_refuses_when_the_manager_is_unreachable() {
        // No manager answer means no confirmed absence: the install refuses
        // at the migration — before any reload, enable, or start — having
        // attempted no legacy stop.
        let mut host = Fake::new();
        host.script.borrow_mut().missing.insert("systemctl".into());

        let (error, _) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();

        assert!(error.contains("refusing to install"), "{error}");
        assert!(
            host.calls_to("stop toka-indexer").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.calls_to("daemon-reload").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.calls_to("start toka-search").is_empty(),
            "{:?}",
            host.calls
        );
    }

    #[test]
    fn install_migration_refuses_after_an_interrupted_probe() {
        // The legacy file is gone but its daemon may stay loaded: an
        // interrupted probe retries once, then refuses instead of starting
        // the new service beside a daemon that still owns its lock.
        let mut host = Fake::new();
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-indexer.service".into());

        let (error, _) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();

        assert!(error.contains("refusing to install"), "{error}");
        assert_eq!(
            host.calls_to("is-active toka-indexer.service").len(),
            2,
            "one probe plus one retry: {:?}",
            host.calls
        );
        assert!(
            host.calls_to("stop toka-indexer").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.calls_to("start toka-search").is_empty(),
            "{:?}",
            host.calls
        );
    }

    #[test]
    fn install_migration_skips_a_confirmed_absent_legacy_service() {
        // No legacy file and the manager confirms nothing loaded (exit-4
        // `inactive`): migration skips cleanly and the install converges.
        let mut host = Fake::new();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            host.unchanged(&outcome)
                .iter()
                .any(|action| action == "no old toka-indexer.service to migrate"),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
        assert!(
            host.calls_to("stop toka-indexer").is_empty(),
            "{:?}",
            host.calls
        );
    }

    #[test]
    fn launchd_install_without_a_reported_program_reports_already_running() {
        // Old `launchctl print` output names no program: with the plist
        // current and no marker set, the install takes the fast path
        // instead of reloading — the program line is never consulted.
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        // Old launchctl: the next `print` names no program.
        host.script.borrow_mut().program = None;
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(host.calls_to("bootout").is_empty(), "{:?}", host.calls);
        assert!(host.calls_to("bootstrap").is_empty(), "{:?}", host.calls);
        assert!(
            host.unchanged(&outcome)
                .iter()
                .any(|action| action == "app.toka.ts is already running"),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
    }

    #[test]
    fn launchd_purge_uninstall_stops_only_once_when_bootout_does_not_unload() {
        // A manager that reports bootout success without unloading: the
        // pre-stop and the main flow must not each report a stop.
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.script.borrow_mut().bootout_noop = true;
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap();

        assert_eq!(host.calls_to("bootout").len(), 1, "{:?}", host.calls);
        assert_eq!(
            host.changed(&outcome)
                .iter()
                .filter(|action| *action == "stopped app.toka.ts")
                .count(),
            1,
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(
            !plist_path(&host.home()).exists(),
            "the plist is still removed"
        );
    }

    #[test]
    fn systemd_purge_uninstall_stops_only_once_when_stop_does_not_stop() {
        // A manager that acks stop without stopping: the single threaded
        // probe plus the pre-stop flag stop — and report — once, like the
        // launchd flow.
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.script.borrow_mut().stop_noop = true;
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap();

        assert_eq!(
            host.calls_to("is-active toka-search.service").len(),
            1,
            "one probe threaded through both phases: {:?}",
            host.calls
        );
        assert_eq!(
            host.calls_to("stop toka-search.service").len(),
            1,
            "{:?}",
            host.calls
        );
        assert_eq!(
            host.changed(&outcome)
                .iter()
                .filter(|action| *action == "stopped toka-search.service")
                .count(),
            1,
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(
            !unit_path(&host.config_dir()).exists(),
            "the unit is still removed"
        );
    }

    #[test]
    fn reset_data_paths_covers_both_thumbnail_cache_roots() {
        // The reset removes the current uid-scoped cache plus the unscoped
        // root uid-scoping orphaned, so upgrade+reset leaves no stale
        // cache behind — and the fresh root is still removed exactly as
        // before. Read-only: building the list touches nothing.
        let paths = reset_data_paths().unwrap();

        for root in [
            crate::thumbnails::cache_output_root(),
            crate::thumbnails::legacy_cache_output_root(),
        ] {
            assert!(
                paths.contains(&ResetTarget::dir(root.clone())),
                "purge misses {} in {:?}",
                root.display(),
                paths
                    .iter()
                    .map(|target| target.path.display().to_string())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn uninstall_ignores_a_unit_under_another_config_dir() {
        // A unit under a config dir `ts` no longer reads — installed
        // before XDG_CONFIG_HOME was honored, or orphaned by a mid-session
        // change — is inert litter systemd never loads: the uninstall
        // reports not-installed and leaves the stale file alone.
        let mut host = Fake::new();
        let stale = host
            .home()
            .join("stale-config")
            .join("systemd")
            .join("user")
            .join("toka-search.service");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"[Unit]").unwrap();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: false },
        )
        .unwrap();

        assert_eq!(
            host.unchanged(&outcome),
            vec!["toka-search.service is not installed".to_owned()]
        );
        assert!(host.changed(&outcome).is_empty());
        assert!(stale.is_file(), "the other dir's file is left alone");
        assert_eq!(
            query_on(&mut host, ServiceManager::Systemd).installed,
            Some(false)
        );
    }

    #[test]
    fn purge_leaves_crash_litter_beside_the_index() {
        // A kill -9 mid-write leaves the overlay's temp sibling beside the
        // database; it is no known index file, so a purge removes the
        // index around it and leaves the litter behind.
        let mut host = Fake::new();
        let database = host.home().join("index.sqlite3");
        for name in [
            "index.sqlite3",
            "index.sqlite3-wal",
            "index.sqlite3-shm",
            "daemon.lock",
            "search-renames.json",
            "search-renames.lock",
        ] {
            std::fs::write(host.home().join(name), b"data").unwrap();
        }
        let litter = host.home().join("search-renames.json.tmp-999-0");
        std::fs::write(&litter, b"partial").unwrap();
        host.purge = crate::owned_index::index_data_files(&database)
            .into_iter()
            .map(ResetTarget::file)
            .collect();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap();

        assert!(!database.exists());
        assert!(litter.is_file(), "tmp litter survives the purge");
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action.contains("index.sqlite3")),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn pinned_service_env_pins_the_effective_locations() {
        let home = Path::new("/home/tester");
        let data_default = "/home/tester/.local/share".to_owned();
        let config_default = "/home/tester/.config".to_owned();
        // Unset: the defaults pin, so a manager-side XDG_DATA_HOME (say
        // /srv/toka-data) cannot split the daemon off the shell's index.
        assert_eq!(
            pinned_service_env(home, None, None),
            vec![
                ("XDG_DATA_HOME".to_owned(), data_default.clone()),
                ("XDG_CONFIG_HOME".to_owned(), config_default.clone()),
            ]
        );
        // Explicit non-defaults are preserved as-is.
        assert_eq!(
            pinned_service_env(home, Some("/data"), Some("/cfg")),
            vec![
                ("XDG_DATA_HOME".to_owned(), "/data".to_owned()),
                ("XDG_CONFIG_HOME".to_owned(), "/cfg".to_owned()),
            ]
        );
        // Set but default-equal: still pinned, since the manager may
        // resolve its own default elsewhere.
        assert_eq!(
            pinned_service_env(
                home,
                Some("/home/tester/.local/share"),
                Some("/home/tester/.config"),
            ),
            vec![
                ("XDG_DATA_HOME".to_owned(), data_default.clone()),
                ("XDG_CONFIG_HOME".to_owned(), config_default.clone()),
            ]
        );
        // Relative and empty values resolve to the default — `dirs` ignores
        // them — so the default pins.
        assert_eq!(
            pinned_service_env(home, Some("relative"), Some("")),
            vec![
                ("XDG_DATA_HOME".to_owned(), data_default.clone()),
                ("XDG_CONFIG_HOME".to_owned(), config_default.clone()),
            ]
        );
        // Mixed: the custom value pins beside the default.
        assert_eq!(
            pinned_service_env(home, Some("/data"), None),
            vec![
                ("XDG_DATA_HOME".to_owned(), "/data".to_owned()),
                ("XDG_CONFIG_HOME".to_owned(), config_default.clone()),
            ]
        );
    }

    #[test]
    fn the_unit_pins_non_default_xdg_locations() {
        let env = vec![
            ("XDG_DATA_HOME".to_owned(), "/data".to_owned()),
            ("XDG_CONFIG_HOME".to_owned(), "/cfg".to_owned()),
        ];
        let unit = render_unit(Path::new("/bin/ts"), &env).unwrap();

        assert!(
            unit.contains("Environment=\"XDG_DATA_HOME=/data\"\n"),
            "{unit}"
        );
        assert!(
            unit.contains("Environment=\"XDG_CONFIG_HOME=/cfg\"\n"),
            "{unit}"
        );
    }

    #[test]
    fn the_unit_without_pinned_env_has_no_environment() {
        let unit = render_unit(Path::new("/bin/ts"), &[]).unwrap();

        assert!(!unit.contains("Environment"), "{unit}");
    }

    #[test]
    fn the_plist_pins_non_default_xdg_locations() {
        let env = vec![
            ("XDG_DATA_HOME".to_owned(), "/data".to_owned()),
            ("XDG_CONFIG_HOME".to_owned(), "/cfg".to_owned()),
        ];
        let plist = render_plist(Path::new("/bin/ts"), &env);

        for (key, value) in [("XDG_DATA_HOME", "/data"), ("XDG_CONFIG_HOME", "/cfg")] {
            assert!(plist.contains(&format!("<key>{key}</key>")), "{plist}");
            assert!(
                plist.contains(&format!("<string>{value}</string>")),
                "{plist}"
            );
        }
    }

    #[test]
    fn the_plist_without_pinned_env_has_no_xdg_keys() {
        let plist = render_plist(Path::new("/bin/ts"), &[]);

        assert!(!plist.contains("XDG_"), "{plist}");
    }

    #[test]
    fn install_writes_the_pinned_environment() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            host.env = vec![("XDG_DATA_HOME".to_owned(), "/data".to_owned())];
            perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();
            let file = match manager {
                ServiceManager::Systemd => unit_path(&host.config_dir()),
                ServiceManager::Launchd => plist_path(&host.home()),
            };
            let contents = std::fs::read_to_string(&file).unwrap();
            match manager {
                ServiceManager::Systemd => assert!(
                    contents.contains("Environment=\"XDG_DATA_HOME=/data\""),
                    "{manager:?}: {contents}"
                ),
                ServiceManager::Launchd => assert!(
                    contents.contains("<key>XDG_DATA_HOME</key>"),
                    "{manager:?}: {contents}"
                ),
            }

            // Without scripted env nothing pins.
            let mut host = Fake::new();
            perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();
            let file = match manager {
                ServiceManager::Systemd => unit_path(&host.config_dir()),
                ServiceManager::Launchd => plist_path(&host.home()),
            };
            let contents = std::fs::read_to_string(&file).unwrap();
            assert!(
                !contents.contains("XDG_DATA_HOME"),
                "{manager:?}: {contents}"
            );
        }
    }

    #[test]
    fn install_pins_the_default_data_dir() {
        // The mismatch scenario: the installing shell leaves XDG_DATA_HOME
        // unset (so it resolves the default) while the manager would
        // resolve its own elsewhere. The install pins the shell's
        // effective default, converging both sides on one index.
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            let home = host.home();
            host.env = pinned_service_env(&home, None, None);
            perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();
            let file = match manager {
                ServiceManager::Systemd => unit_path(&host.config_dir()),
                ServiceManager::Launchd => plist_path(&host.home()),
            };
            let contents = std::fs::read_to_string(&file).unwrap();
            let pinned = home.join(".local/share").to_string_lossy().into_owned();
            match manager {
                ServiceManager::Systemd => assert!(
                    contents.contains(&format!("Environment=\"XDG_DATA_HOME={pinned}\"")),
                    "{manager:?}: {contents}"
                ),
                ServiceManager::Launchd => assert!(
                    contents.contains("<key>XDG_DATA_HOME</key>")
                        && contents.contains(&format!("<string>{pinned}</string>")),
                    "{manager:?}: {contents}"
                ),
            }
        }
    }

    #[test]
    fn start_pushes_through_a_deactivating_service() {
        // Mid-stop is actionable for stop but never skips start: skipping
        // as "already running" would leave the service stopped once the
        // pending stop lands.
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.script
            .borrow_mut()
            .active_word
            .insert("toka-search.service".into(), "deactivating".into());
        host.calls.clear();

        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Start).unwrap();

        assert!(
            !host.calls_to("stop toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            !host.calls_to("start toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert_eq!(
            host.changed(&outcome),
            vec![
                "stopped toka-search.service".to_owned(),
                "started toka-search.service".to_owned(),
            ]
        );
        assert!(host.unchanged(&outcome).is_empty());
    }

    #[test]
    fn install_pushes_through_a_deactivating_service() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.script
            .borrow_mut()
            .active_word
            .insert("toka-search.service".into(), "deactivating".into());
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            !host.calls_to("stop toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            !host.calls_to("start toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "restarted toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(
            host.unchanged(&outcome)
                .iter()
                .all(|action| !action.contains("already running")),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
    }

    #[test]
    fn install_claims_a_change_only_for_a_confirmed_stopped_start() {
        // A probe that confirmed stopped plus a successful start is a real
        // state change.
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.script
            .borrow_mut()
            .active
            .insert("toka-search.service".into(), false);
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            !host.calls_to("start toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "started toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn install_reports_an_unverified_start_without_claiming_a_change() {
        // An unreadable probe may hide an already-active service, in which
        // case the start no-ops: the request is still sent, but no state
        // change is claimed.
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-search.service".into());
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            !host.calls_to("start toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.unchanged(&outcome).iter().any(
                |action| action == "systemd accepted the start request for toka-search.service"
            ),
            "{}",
            host.unchanged(&outcome).join("\n")
        );
        assert!(
            host.changed(&outcome)
                .iter()
                .all(|action| action != "started toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_install_lock_is_held_while_an_installer_runs() {
        let root = tempfile::tempdir().unwrap();
        let service = root.path().join("toka-search.service");

        let held = acquire_install_lock(&service).unwrap();

        assert!(
            try_acquire_install_lock(&service).unwrap().is_none(),
            "a second installer waits while the first holds the lock"
        );
        drop(held);
        assert!(
            try_acquire_install_lock(&service).unwrap().is_some(),
            "releasing the lock admits the waiter"
        );
    }

    #[test]
    fn restart_stops_before_starting_even_when_the_probe_says_stopped() {
        // A bare start would no-op while a stale daemon the probe missed
        // kept running, so restart always stops first.
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Stop).unwrap();
        host.calls.clear();

        // A truly-stopped service: the stop is a verified no-op, and the
        // service still ends up running.
        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Restart).unwrap();

        assert!(
            !host.calls_to("stop toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            !host.calls_to("start toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert_eq!(
            host.unchanged(&outcome),
            vec!["toka-search.service is already stopped".to_owned()]
        );
        assert!(host.script.borrow().active["toka-search.service"]);

        // The probe says stopped while a stale daemon runs: the stop kills
        // it, and the start brings a fresh one up.
        host.script
            .borrow_mut()
            .active_word
            .insert("toka-search.service".into(), "inactive".into());
        host.calls.clear();

        let outcome = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Restart).unwrap();

        assert!(
            !host.calls_to("stop toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            !host.calls_to("start toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(host.script.borrow().active["toka-search.service"]);
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "started toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn launchd_install_failing_at_bootstrap_reports_the_bootout() {
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.executable = host.home().join("elsewhere").join("ts");
        let plist = plist_path(&host.home());
        host.script.borrow_mut().fail.insert(format!(
            "launchctl bootstrap gui/{} {}",
            host.uid,
            plist_display(&plist)
        ));

        // The bootout succeeds, then the bootstrap fails: the partial
        // report names the stop instead of omitting it.
        let (error, partial) = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();

        assert!(error.contains("bootstrap"), "{error}");
        assert!(
            host.changed(&partial)
                .iter()
                .any(|action| action == "stopped app.toka.ts"),
            "{}",
            host.changed(&partial).join("\n")
        );
        let marker = marker_path(&plist_path(&host.home()));
        assert!(marker.is_file(), "the failed reload leaves its marker");
        host.script.borrow_mut().fail.clear();

        // The retry completes the pending reload.
        let outcome = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "started app.toka.ts"),
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(!marker.exists(), "the verified reload clears it");
    }

    #[test]
    fn same_exe_settings_change_retries_through_a_failed_stop() {
        // The executable never moves — only an `Environment` line is new —
        // so executable comparison would call the manager current. The
        // marker carries the pending reload instead: the reload succeeds,
        // the stop fails with the old process still running, and the retry
        // completes the turnover instead of reporting "already running".
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.env = vec![("XDG_DATA_HOME".to_owned(), "/data".to_owned())];
        host.script
            .borrow_mut()
            .fail
            .insert("systemctl --user stop toka-search.service".to_owned());

        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();
        let marker = marker_path(&unit_path(&host.config_dir()));
        assert!(marker.is_file(), "the failed stop leaves its marker");
        assert!(
            host.script.borrow().active["toka-search.service"],
            "the old process still runs"
        );
        host.script.borrow_mut().fail.clear();
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            !host.calls_to("stop toka-search.service").is_empty(),
            "{:?}",
            host.calls
        );
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "restarted toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(!marker.exists(), "the verified reload clears it");
        let unit = std::fs::read_to_string(unit_path(&host.config_dir())).unwrap();
        assert!(
            unit.contains("Environment=\"XDG_DATA_HOME=/data\""),
            "{unit}"
        );
    }

    #[test]
    fn install_with_a_leftover_marker_completes_the_pending_reload() {
        // A crash between the write and the reload: new content on disk
        // plus a set marker, with the manager still on the old one. The
        // next install reloads even though the file is already current.
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();
            host.executable = host.home().join("elsewhere").join("ts");
            let file = match manager {
                ServiceManager::Systemd => unit_path(&host.config_dir()),
                ServiceManager::Launchd => plist_path(&host.home()),
            };
            let rendered = match manager {
                ServiceManager::Systemd => {
                    render_unit(&host.executable, &host.service_env()).unwrap()
                }
                ServiceManager::Launchd => render_plist(&host.executable, &host.service_env()),
            };
            set_reload_marker(&marker_path(&file)).unwrap();
            assert!(write_service_file(&file, &rendered).unwrap());
            let marker = marker_path(&file);
            host.calls.clear();

            let outcome =
                perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();

            match manager {
                ServiceManager::Systemd => {
                    assert!(
                        !host.calls_to("stop toka-search.service").is_empty(),
                        "{manager:?}: {:?}",
                        host.calls
                    );
                    assert!(
                        host.changed(&outcome)
                            .iter()
                            .any(|action| action == "restarted toka-search.service"),
                        "{manager:?}: {}",
                        host.changed(&outcome).join("\n")
                    );
                }
                ServiceManager::Launchd => {
                    assert!(!host.calls_to("bootout").is_empty(), "{:?}", host.calls);
                    assert!(!host.calls_to("bootstrap").is_empty(), "{:?}", host.calls);
                    assert!(
                        host.changed(&outcome)
                            .iter()
                            .any(|action| action == "stopped app.toka.ts"),
                        "{manager:?}: {}",
                        host.changed(&outcome).join("\n")
                    );
                }
            }
            assert!(
                host.unchanged(&outcome)
                    .iter()
                    .all(|action| !action.contains("already running")),
                "{manager:?}: {}",
                host.unchanged(&outcome).join("\n")
            );
            assert!(!marker.exists(), "the verified reload clears it");
        }
    }

    #[test]
    fn install_with_an_unverifiable_reload_keeps_the_marker() {
        // Stop and start both succeed, but the probe cannot confirm the
        // new file loaded: the install fails instead of claiming
        // convergence, the marker stays set, and a later retry — with the
        // manager answering again — completes the reload.
        let mut host = Fake::new();
        perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();
        host.executable = host.home().join("elsewhere").join("ts");
        host.script
            .borrow_mut()
            .active_unknown
            .insert("toka-search.service".into());

        let (error, _) = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();

        assert!(error.contains("could not verify"), "{error}");
        assert!(error.contains("pending"), "{error}");
        let marker = marker_path(&unit_path(&host.config_dir()));
        assert!(marker.is_file(), "the unverified reload keeps its marker");
        host.script.borrow_mut().active_unknown.clear();
        host.calls.clear();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "restarted toka-search.service"),
            "{}",
            host.changed(&outcome).join("\n")
        );
        assert!(!marker.exists(), "the verified retry clears it");
    }

    #[test]
    fn uninstall_removes_a_leftover_marker() {
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();
            let file = match manager {
                ServiceManager::Systemd => unit_path(&host.config_dir()),
                ServiceManager::Launchd => plist_path(&host.home()),
            };
            let marker = marker_path(&file);
            set_reload_marker(&marker).unwrap();

            perform_on(
                &mut host,
                manager,
                ServiceOp::Uninstall { purge_data: false },
            )
            .unwrap();

            assert!(!file.exists(), "{manager:?}");
            assert!(!marker.exists(), "{manager:?}: the marker goes too");
        }
    }

    #[test]
    #[cfg(unix)]
    fn install_keeps_a_symlinked_service_file_when_staging_fails() {
        // The link is swapped out only by the final rename: when staging
        // fails, the old link — and its target's contents — survive, and a
        // retry past the unblocked staging completes the install.
        for manager in [ServiceManager::Systemd, ServiceManager::Launchd] {
            let mut host = Fake::new();
            let file = match manager {
                ServiceManager::Systemd => unit_path(&host.config_dir()),
                ServiceManager::Launchd => plist_path(&host.home()),
            };
            let target = host.home().join("dotfiles-unit");
            std::fs::write(&target, b"[stale]").unwrap();
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&target, &file).unwrap();
            std::fs::create_dir(sibling_temp_file(&file)).unwrap();

            let (error, _) =
                perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap_err();

            assert!(error.contains("could not write"), "{manager:?}: {error}");
            assert!(
                std::fs::symlink_metadata(&file)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{manager:?}: the link survives"
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"[stale]", "{manager:?}");

            std::fs::remove_dir(sibling_temp_file(&file)).unwrap();
            perform_on(&mut host, manager, ServiceOp::Install { link: false }).unwrap();

            assert!(
                !std::fs::symlink_metadata(&file)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{manager:?}: the retry replaces the link"
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"[stale]", "{manager:?}");
        }
    }
}
