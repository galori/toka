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
/// once installed, whether the manager has it enabled and running. The two
/// `Option`s are `None` where the manager cannot say (no manager, or its
/// answer was unreadable) — and always where nothing is installed, since a
/// hand-started daemon may still run there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServiceState {
    pub installed: bool,
    pub enabled: Option<bool>,
    pub running: Option<bool>,
}

impl ServiceState {
    pub fn unknown() -> Self {
        Self {
            installed: false,
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
    /// The `ts` executable the service files point at.
    fn executable(&self) -> PathBuf;
    /// The login user id, for `launchctl`'s `gui/<uid>` domain.
    fn uid(&self) -> u32;
    /// The login name, for `loginctl enable-linger`.
    fn user(&self) -> Option<String>;
    fn run(&mut self, program: &str, args: &[&str]) -> Result<CommandResult, String>;
    /// The index data an uninstall purges: the index folder, the logs, the
    /// thumbnail cache. Never media. A host method (not a free function)
    /// so tests point it at scratch instead of the real index.
    fn purge_paths(&mut self) -> Result<Vec<PathBuf>, String>;
}

/// The real machine.
pub struct SystemHost {
    home: PathBuf,
    executable: PathBuf,
    uid: u32,
    user: Option<String>,
}

impl SystemHost {
    pub fn new() -> Result<Self, String> {
        let home = dirs::home_dir()
            .ok_or_else(|| "Toka could not find your home directory.".to_owned())?;
        let executable = std::env::current_exe()
            .map_err(|error| format!("could not find the ts executable: {error}"))?;
        #[cfg(unix)]
        let uid = unsafe { libc::getuid() };
        #[cfg(not(unix))]
        let uid = 0;
        let user = std::env::var("USER").ok().filter(|user| !user.is_empty());
        Ok(Self {
            home,
            executable,
            uid,
            user,
        })
    }
}

impl Host for SystemHost {
    fn home(&self) -> PathBuf {
        self.home.clone()
    }

    fn executable(&self) -> PathBuf {
        self.executable.clone()
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

    fn purge_paths(&mut self) -> Result<Vec<PathBuf>, String> {
        purge_data_paths()
    }
}

/// The index data `uninstall --purge-data` removes: the index folder (the
/// database, the rename overlay, the daemon's state), every log file, and
/// the thumbnail cache. Never media, tags, or add-ons — the same set the
/// app's reset removes, minus the app's own caches.
fn purge_data_paths() -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    if let Some(directory) = crate::owned_index::database_path()?.parent() {
        paths.push(directory.to_path_buf());
    }
    paths.extend(crate::search_log::reset_paths());
    paths.push(crate::thumbnails::cache_output_root());
    Ok(paths)
}

/// The unit file `ts install` writes, beside where the app installer put
/// the old one.
pub fn unit_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join("systemd")
        .join("user")
        .join(crate::config::TS_SERVICE_NAME)
}

/// The old unit file, written by the app installer and migrated away by
/// `ts install`.
fn legacy_unit_path(home: &Path) -> PathBuf {
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
/// `ts daemon run` instead of `toka --indexer`.
pub fn render_unit(executable: &Path) -> String {
    format!(
        "[Unit]\nDescription=Toka search indexer\n\n[Service]\nExecStart=\"{}\" daemon run\nRestart=on-failure\nRestartSec=2\nNice=10\nIOSchedulingClass=idle\n\n[Install]\nWantedBy=default.target\n",
        systemd_escape(executable),
    )
}

/// Quotes one path for a systemd `ExecStart=` line, exactly as the app
/// installer does: backslashes and double quotes escaped, wrapped in
/// double quotes.
fn systemd_escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// The launchd LaunchAgent plist: kept alive in the background with idle
/// I/O priority, started at login, running `ts daemon run` with no shell
/// in between.
pub fn render_plist(executable: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n\t<key>Label</key>\n\t<string>{}</string>\n\t<key>ProgramArguments</key>\n\t<array>\n\t\t<string>{}</string>\n\t\t<string>daemon</string>\n\t\t<string>run</string>\n\t</array>\n\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n\t<key>ProcessType</key>\n\t<string>Background</string>\n\t<key>LowPriorityIO</key>\n\t<true/>\n</dict>\n</plist>\n",
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
/// the state it already found.
pub fn perform(host: &mut impl Host, op: ServiceOp) -> Result<Outcome, String> {
    match ServiceManager::current() {
        Some(manager) => perform_on(host, manager, op),
        None => Err("service management is only supported on Linux and macOS.".to_owned()),
    }
}

/// [`perform`] against an explicit manager, so both flows are testable on
/// either OS.
pub fn perform_on(
    host: &mut impl Host,
    manager: ServiceManager,
    op: ServiceOp,
) -> Result<Outcome, String> {
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

/// Stops every indexer service that is running — the `ts` one and, while
/// the migration is still rolling out, the old `toka --indexer` one — so a
/// reset cannot delete state out from under a live supervisor. A service
/// that is not running needs no stopping; a manager that cannot be reached
/// cannot be running anything, so an unreadable answer also proceeds.
/// Only a running service that refuses to stop fails, and then the reset
/// must not delete anything (see [`crate::reset::apply`]).
pub fn stop_indexer_services() -> Result<(), String> {
    let mut host = SystemHost::new()?;
    let Some(manager) = ServiceManager::current() else {
        return Ok(());
    };
    stop_for_reset(&mut host, manager)
}

/// [`stop_indexer_services`] against an explicit manager, for tests.
fn stop_for_reset(host: &mut impl Host, manager: ServiceManager) -> Result<(), String> {
    match manager {
        ServiceManager::Systemd => {
            for unit in [
                crate::config::TS_SERVICE_NAME,
                crate::config::INDEXER_SERVICE_NAME,
            ] {
                if systemd_active(host, unit) == Some(true) {
                    systemd_command(host, &["stop", unit])?;
                }
            }
            Ok(())
        }
        ServiceManager::Launchd => {
            if launchd_bootstrapped(host) == Some(true) {
                launchd_command(host, &["bootout", &launchd_target(host)])?;
            }
            Ok(())
        }
    }
}

fn query_systemd(host: &mut impl Host) -> ServiceState {
    let installed = unit_path(&host.home()).is_file();
    if !installed {
        return ServiceState::unknown();
    }
    ServiceState {
        installed: true,
        enabled: systemd_enabled(host, crate::config::TS_SERVICE_NAME),
        running: systemd_active(host, crate::config::TS_SERVICE_NAME),
    }
}

/// Whether a unit is active: `is-active` answers 0 for active and 3 for
/// inactive or failed, exactly as the old `indexer_running` read it.
/// Anything else (no user bus, signals, no systemctl at all) means systemd
/// cannot say.
fn systemd_active(host: &mut impl Host, unit: &str) -> Option<bool> {
    let result = host.run("systemctl", &["--user", "is-active", "--quiet", unit]);
    match result {
        Ok(result) => match result.code {
            Some(0) => Some(true),
            Some(3) => Some(false),
            _ => None,
        },
        Err(_) => None,
    }
}

/// Whether a unit is enabled. `is-enabled` exits 0 for an enabled unit;
/// anything else it can name (disabled, static, masked, missing) reads as
/// not enabled. Only a systemctl that cannot run at all reads as unknown.
fn systemd_enabled(host: &mut impl Host, unit: &str) -> Option<bool> {
    let result = host.run("systemctl", &["--user", "is-enabled", unit]);
    match result {
        Ok(result) => Some(result.success() && result.stdout.trim().starts_with("enabled")),
        Err(_) => None,
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

fn install_systemd(host: &mut impl Host, link: bool) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let home = host.home();
    let unit = unit_path(&home);

    let written = write_service_file(&unit, &render_unit(&host.executable()))?;
    if written {
        outcome.did(format!("wrote {}", display_path(&home, &unit)));
    } else {
        outcome.already("service file already up to date".to_owned());
    }

    // The old service stops before anything new starts: it holds the
    // legacy daemon lock, and a new daemon started first would yield to
    // it and crash-loop under `Restart=on-failure`.
    let legacy_removed = migrate_legacy_service(host, &mut outcome)?;

    if written || legacy_removed {
        systemd_command(host, &["daemon-reload"])?;
        outcome.did("reloaded systemd".to_owned());
    }

    if systemd_enabled(host, crate::config::TS_SERVICE_NAME) == Some(true) {
        outcome.already(format!(
            "{} is already enabled",
            crate::config::TS_SERVICE_NAME
        ));
    } else {
        systemd_command(host, &["enable", crate::config::TS_SERVICE_NAME])?;
        outcome.did(format!("enabled {}", crate::config::TS_SERVICE_NAME));
    }

    if systemd_active(host, crate::config::TS_SERVICE_NAME) == Some(true) {
        outcome.already(format!(
            "{} is already running",
            crate::config::TS_SERVICE_NAME
        ));
    } else {
        systemd_command(host, &["start", crate::config::TS_SERVICE_NAME])?;
        outcome.did(format!("started {}", crate::config::TS_SERVICE_NAME));
    }

    ensure_linger(host, &mut outcome);

    if link {
        link_ts(host, &mut outcome)?;
    }
    Ok(outcome)
}

/// Stops, disables, and removes the old `toka-indexer.service`, returning
/// whether its file was removed. A running old service that refuses to
/// stop fails the install: starting the new daemon next to it would only
/// watch it yield and crash-loop.
fn migrate_legacy_service(host: &mut impl Host, outcome: &mut Outcome) -> Result<bool, String> {
    let legacy = crate::config::INDEXER_SERVICE_NAME;
    let file = legacy_unit_path(&host.home());
    let active = systemd_active(host, legacy) == Some(true);
    let present = file.is_file();
    if !active && !present {
        outcome.already(format!("no old {legacy} to migrate"));
        return Ok(false);
    }
    if active {
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
    Ok(false)
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

fn uninstall_systemd(host: &mut impl Host, purge_data: bool) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let mut removed_file = false;
    removed_file |= remove_systemd_service(
        host,
        &mut outcome,
        crate::config::TS_SERVICE_NAME,
        &unit_path(&host.home()),
        true,
    )?;
    removed_file |= remove_systemd_service(
        host,
        &mut outcome,
        crate::config::INDEXER_SERVICE_NAME,
        &legacy_unit_path(&host.home()),
        false,
    )?;
    if removed_file {
        systemd_command(host, &["daemon-reload"])?;
        outcome.did("reloaded systemd".to_owned());
    }
    unlink_ts(host, &mut outcome)?;
    if purge_data {
        purge_index_data(host, &mut outcome)?;
    }
    Ok(outcome)
}

/// Stops, disables, and removes one unit, returning whether its file was
/// removed. A running service that refuses to stop fails the uninstall:
/// removing the unit under it would leave the daemon running unmanaged.
/// The `ts` unit reports its own absence; the legacy one stays silent when
/// there was never anything to migrate.
fn remove_systemd_service(
    host: &mut impl Host,
    outcome: &mut Outcome,
    unit: &str,
    file: &Path,
    report_absent: bool,
) -> Result<bool, String> {
    let active = systemd_active(host, unit) == Some(true);
    let present = file.is_file();
    if !active && !present {
        if report_absent {
            outcome.already(format!("{unit} is not installed"));
        }
        return Ok(false);
    }
    if active {
        systemd_command(host, &["stop", unit])?;
        outcome.did(format!("stopped {unit}"));
    }
    if present {
        systemd_command(host, &["disable", unit])?;
        outcome.did(format!("disabled {unit}"));
        std::fs::remove_file(file)
            .map_err(|error| format!("could not remove {}: {error}", file.display()))?;
        outcome.did(format!("removed {}", display_path(&host.home(), file)));
        return Ok(true);
    }
    Ok(false)
}

fn start_systemd(host: &mut impl Host) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let unit = crate::config::TS_SERVICE_NAME;
    if !unit_path(&host.home()).is_file() {
        return Err(format!("{unit} is not installed; run `ts install` first."));
    }
    if systemd_active(host, unit) == Some(true) {
        outcome.already(format!("{unit} is already running"));
    } else {
        systemd_command(host, &["start", unit])?;
        outcome.did(format!("started {unit}"));
    }
    Ok(outcome)
}

fn stop_systemd(host: &mut impl Host) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let unit = crate::config::TS_SERVICE_NAME;
    if systemd_active(host, unit) == Some(true) {
        systemd_command(host, &["stop", unit])?;
        outcome.did(format!("stopped {unit}"));
        return Ok(outcome);
    }
    if unit_path(&host.home()).is_file() {
        outcome.already(format!("{unit} is already stopped"));
    } else {
        outcome.already(format!("{unit} is not installed, nothing to stop"));
    }
    Ok(outcome)
}

fn restart_systemd(host: &mut impl Host) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let unit = crate::config::TS_SERVICE_NAME;
    if !unit_path(&host.home()).is_file() {
        return Err(format!("{unit} is not installed; run `ts install` first."));
    }
    // Stop plus start, not `systemctl restart`: each half reports what it
    // did, and a stopped service still ends up running. The new daemon
    // process publishes a fresh start time, so `ts status` uptime resets.
    if systemd_active(host, unit) == Some(true) {
        systemd_command(host, &["stop", unit])?;
        outcome.did(format!("stopped {unit}"));
    } else {
        outcome.already(format!("{unit} is already stopped"));
    }
    systemd_command(host, &["start", unit])?;
    outcome.did(format!("started {unit}"));
    Ok(outcome)
}

/// The user's GUI domain service target, e.g. `gui/501/app.toka.ts`.
fn launchd_target(host: &impl Host) -> String {
    format!("gui/{}/{}", host.uid(), crate::config::TS_LAUNCHD_LABEL)
}

fn query_launchd(host: &mut impl Host) -> ServiceState {
    let installed = plist_path(&host.home()).is_file();
    if !installed {
        return ServiceState::unknown();
    }
    // launchd has no enabled bit of its own for agents: bootstrapped is
    // loaded, and a loaded agent with `RunAtLoad` runs. `launchctl print`
    // answers both at once — exit 0 means bootstrapped, and its `state`
    // line says whether the job runs right now.
    match launchd_probe(host) {
        None => ServiceState {
            installed: true,
            enabled: None,
            running: None,
        },
        Some((bootstrapped, running)) => ServiceState {
            installed: true,
            enabled: Some(bootstrapped),
            running: bootstrapped.then_some(running).flatten(),
        },
    }
}

/// One `launchctl print`: whether the agent is bootstrapped, and whether
/// its job runs. `None` is a launchctl that cannot run at all; an agent
/// that is not bootstrapped may still have a hand-started daemon behind
/// it, so its running answer stays unknown rather than false.
fn launchd_probe(host: &mut impl Host) -> Option<(bool, Option<bool>)> {
    let target = launchd_target(host);
    let result = host.run("launchctl", &["print", &target]).ok()?;
    if !result.success() {
        return Some((false, None));
    }
    Some((true, launchd_running_state(&result.stdout)))
}

fn launchd_bootstrapped(host: &mut impl Host) -> Option<bool> {
    launchd_probe(host).map(|(bootstrapped, _)| bootstrapped)
}

/// Pulls the job state out of `launchctl print` output, which carries a
/// `state = running` line for a live job (`exited`, `spawn scheduled`,
/// and the rest read as not running). No state line means launchd would
/// not say.
fn launchd_running_state(print_output: &str) -> Option<bool> {
    for line in print_output.lines() {
        let line = line.trim();
        let Some((_, state)) = line.split_once('=') else {
            continue;
        };
        if line.starts_with("state") {
            return Some(state.trim().trim_end_matches(';').trim() == "running");
        }
    }
    None
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

fn install_launchd(host: &mut impl Host, link: bool) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let home = host.home();
    let plist = plist_path(&home);
    let label = crate::config::TS_LAUNCHD_LABEL;

    let written = write_service_file(&plist, &render_plist(&host.executable()))?;
    if written {
        outcome.did(format!("wrote {}", display_path(&home, &plist)));
    } else {
        outcome.already("service file already up to date".to_owned());
    }

    let Some((bootstrapped, running)) = launchd_probe(host) else {
        return Err("could not ask launchd about the service.".to_owned());
    };
    if bootstrapped && !written {
        match running {
            Some(true) => outcome.already(format!("{label} is already running")),
            Some(false) => {
                launchd_command(host, &["kickstart", "-k", &launchd_target(host)])?;
                outcome.did(format!("started {label}"));
            }
            None => outcome.already(format!("{label} is already loaded")),
        }
    } else if bootstrapped {
        // The plist changed under a loaded agent: unload and load again so
        // the new one takes effect now, not at the next login.
        launchd_command(host, &["bootout", &launchd_target(host)])?;
        launchd_command(
            host,
            &[
                "bootstrap",
                &format!("gui/{}", host.uid()),
                &plist_display(&plist),
            ],
        )?;
        outcome.did(format!("reloaded {label}"));
    } else {
        launchd_command(
            host,
            &[
                "bootstrap",
                &format!("gui/{}", host.uid()),
                &plist_display(&plist),
            ],
        )?;
        outcome.did(format!("started {label}"));
    }

    if link {
        link_ts(host, &mut outcome)?;
    }
    Ok(outcome)
}

/// The plist path as `launchctl bootstrap` takes it: absolute, as written.
/// Lossy on purpose, like every path this CLI handles: a non-UTF-8 home
/// must not break the invocation.
fn plist_display(plist: &Path) -> String {
    plist.to_string_lossy().into_owned()
}

fn uninstall_launchd(host: &mut impl Host, purge_data: bool) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let home = host.home();
    let plist = plist_path(&home);
    let label = crate::config::TS_LAUNCHD_LABEL;
    let present = plist.is_file();

    match launchd_probe(host) {
        None if !present => {
            outcome.already(format!("{label} is not installed"));
        }
        None => {
            return Err("could not ask launchd about the service.".to_owned());
        }
        Some((bootstrapped, _)) => {
            if bootstrapped {
                launchd_command(host, &["bootout", &launchd_target(host)])?;
                outcome.did(format!("stopped {label}"));
            }
            if present {
                std::fs::remove_file(&plist)
                    .map_err(|error| format!("could not remove {}: {error}", plist.display()))?;
                outcome.did(format!("removed {}", display_path(&home, &plist)));
            }
            if !bootstrapped && !present {
                outcome.already(format!("{label} is not installed"));
            }
        }
    }
    unlink_ts(host, &mut outcome)?;
    if purge_data {
        purge_index_data(host, &mut outcome)?;
    }
    Ok(outcome)
}

fn start_launchd(host: &mut impl Host) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let label = crate::config::TS_LAUNCHD_LABEL;
    if !plist_path(&host.home()).is_file() {
        return Err(format!("{label} is not installed; run `ts install` first."));
    }
    let Some((bootstrapped, running)) = launchd_probe(host) else {
        return Err("could not ask launchd about the service.".to_owned());
    };
    if running == Some(true) {
        outcome.already(format!("{label} is already running"));
    } else if bootstrapped {
        launchd_command(host, &["kickstart", "-k", &launchd_target(host)])?;
        outcome.did(format!("started {label}"));
    } else {
        launchd_command(
            host,
            &[
                "bootstrap",
                &format!("gui/{}", host.uid()),
                &plist_display(&plist_path(&host.home())),
            ],
        )?;
        outcome.did(format!("started {label}"));
    }
    Ok(outcome)
}

fn stop_launchd(host: &mut impl Host) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let label = crate::config::TS_LAUNCHD_LABEL;
    let installed = plist_path(&host.home()).is_file();
    match launchd_probe(host) {
        None if !installed => {
            outcome.already(format!("{label} is not installed, nothing to stop"));
        }
        None => {
            return Err("could not ask launchd about the service.".to_owned());
        }
        Some((bootstrapped, _)) => {
            if bootstrapped {
                launchd_command(host, &["bootout", &launchd_target(host)])?;
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

fn restart_launchd(host: &mut impl Host) -> Result<Outcome, String> {
    let mut outcome = Outcome::default();
    let label = crate::config::TS_LAUNCHD_LABEL;
    let home = host.home();
    if !plist_path(&home).is_file() {
        return Err(format!("{label} is not installed; run `ts install` first."));
    }
    // Stop plus start, mirroring the systemd flow: each half reports what
    // it did, a stopped agent still ends up running, and the new daemon
    // process resets `ts status` uptime.
    let Some((bootstrapped, _)) = launchd_probe(host) else {
        return Err("could not ask launchd about the service.".to_owned());
    };
    if bootstrapped {
        launchd_command(host, &["bootout", &launchd_target(host)])?;
        outcome.did(format!("stopped {label}"));
    } else {
        outcome.already(format!("{label} is already stopped"));
    }
    launchd_command(
        host,
        &[
            "bootstrap",
            &format!("gui/{}", host.uid()),
            &plist_display(&plist_path(&home)),
        ],
    )?;
    outcome.did(format!("started {label}"));
    Ok(outcome)
}

/// Writes a service file, creating its parent. An identical file is left
/// alone, so a second install reports already-held state instead of
/// churning mtimes; the return says whether anything was written.
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
    std::fs::write(path, contents)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    Ok(true)
}

/// Links `ts` into `~/.local/bin`, so install scripts and shells find the
/// same executable the service runs. A link that already points here is
/// left alone; anything else at that path is replaced.
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
    match std::fs::symlink_metadata(&link) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(format!(
                "could not link {}: a directory is in the way",
                display_path(&home, &link)
            ));
        }
        Ok(_) => std::fs::remove_file(&link)
            .map_err(|error| format!("could not replace {}: {error}", link.display()))?,
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

/// Whether `link` is a symlink resolving to `executable`, following the
/// `restart` precedent: canonical paths compared, falling back to the raw
/// spellings when either side cannot resolve.
fn link_points_at(link: &Path, executable: &Path) -> bool {
    let Ok(target) = std::fs::read_link(link) else {
        return false;
    };
    // A relative link resolves against its own parent, not the working
    // directory.
    let target = if target.is_absolute() {
        target
    } else if let Some(parent) = link.parent() {
        parent.join(target)
    } else {
        target
    };
    match (target.canonicalize(), executable.canonicalize()) {
        (Ok(target), Ok(executable)) => target == executable,
        _ => target == *executable,
    }
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
/// skips them; anything left behind fails, naming what resisted.
fn purge_index_data(host: &mut impl Host, outcome: &mut Outcome) -> Result<(), String> {
    let home = host.home();
    let plan = crate::reset::ResetPlan::new(host.purge_paths()?);
    let report = crate::reset::apply(&plan);
    for removed in &report.removed {
        outcome.did(format!("removed {}", display_path(&home, removed)));
    }
    if !report.failures.is_empty() {
        return Err(report.failures.join("\n"));
    }
    if report.removed.is_empty() {
        outcome.already("no index data to remove".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;

    /// The scripted manager state behind a [`Fake`]: which units are
    /// active/enabled, whether the agent is bootstrapped and running,
    /// whether lingering is on, and which invocations fail.
    #[derive(Default)]
    struct Script {
        active: HashMap<String, bool>,
        enabled: HashMap<String, bool>,
        bootstrapped: bool,
        job_running: bool,
        lingering: bool,
        fail: HashSet<String>,
        missing: HashSet<String>,
    }

    impl Script {
        fn answer(&mut self, program: &str, args: &[&str]) -> Result<CommandResult, String> {
            if self.missing.contains(program) {
                return Err(format!("could not run {program}"));
            }
            let call = format!("{program} {}", args.join(" "));
            if self.fail.contains(&call) {
                return Ok(CommandResult {
                    code: Some(1),
                    stdout: String::new(),
                    stderr: "injected failure".to_owned(),
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
                ["--user", "is-active", "--quiet", unit] => out(
                    if self.active.get(*unit) == Some(&true) {
                        0
                    } else {
                        3
                    },
                    "",
                ),
                ["--user", "is-enabled", unit] => {
                    if self.enabled.get(*unit) == Some(&true) {
                        out(0, "enabled\n")
                    } else {
                        out(1, "disabled\n")
                    }
                }
                ["--user", "start", unit] => {
                    self.active.insert((*unit).to_owned(), true);
                    out(0, "")
                }
                ["--user", "stop", unit] => {
                    self.active.insert((*unit).to_owned(), false);
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
                        out(
                            0,
                            &format!("gui/501/app.toka.ts = {{\n\tstate = {state};\n}}\n"),
                            "",
                        )
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
                    self.bootstrapped = false;
                    self.job_running = false;
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
        uid: u32,
        user: Option<String>,
        calls: Vec<String>,
        script: Rc<RefCell<Script>>,
        purge: Vec<PathBuf>,
    }

    impl Fake {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let executable = temp.path().join("bin").join("ts");
            std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
            std::fs::write(&executable, b"ts").unwrap();
            Self {
                temp,
                executable,
                uid: 501,
                user: Some("tester".to_owned()),
                calls: Vec::new(),
                script: Rc::new(RefCell::new(Script::default())),
                purge: Vec::new(),
            }
        }

        fn home(&self) -> PathBuf {
            self.temp.path().to_path_buf()
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

        fn executable(&self) -> PathBuf {
            self.executable.clone()
        }

        fn uid(&self) -> u32 {
            self.uid
        }

        fn user(&self) -> Option<String> {
            self.user.clone()
        }

        fn run(&mut self, program: &str, args: &[&str]) -> Result<CommandResult, String> {
            self.calls.push(format!("{program} {}", args.join(" ")));
            self.script.borrow_mut().answer(program, args)
        }

        fn purge_paths(&mut self) -> Result<Vec<PathBuf>, String> {
            Ok(self.purge.clone())
        }
    }

    #[test]
    fn the_unit_runs_ts_daemon_run_with_idle_scheduling() {
        let unit = render_unit(Path::new("/home/tester/.local/bin/ts"));

        assert!(unit.contains("Description=Toka search indexer\n"), "{unit}");
        assert!(
            unit.contains("ExecStart=\"/home/tester/.local/bin/ts\" daemon run\n"),
            "{unit}"
        );
        for line in [
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
        let unit = render_unit(Path::new("/weird\\dir/ts\"x"));

        assert!(
            unit.contains("ExecStart=\"/weird\\\\dir/ts\\\"x\" daemon run\n"),
            "{unit}"
        );
    }

    #[test]
    fn the_plist_keeps_a_background_agent_alive_at_login() {
        let plist = render_plist(Path::new("/Users/tester/.local/bin/ts"));

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
        let plist = render_plist(Path::new("/tmp/a&b/<ts>"));

        assert!(plist.contains("/tmp/a&amp;b/&lt;ts&gt;"), "{plist}");
    }

    #[test]
    fn systemd_query_without_a_unit_file_runs_no_commands() {
        let mut host = Fake::new();

        let state = query_on(&mut host, ServiceManager::Systemd);

        assert_eq!(state, ServiceState::unknown());
        assert!(host.calls.is_empty());
    }

    #[test]
    fn systemd_query_reports_enabled_and_running() {
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.home()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.home()), b"[Unit]").unwrap();
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
                installed: true,
                enabled: Some(true),
                running: Some(true),
            }
        );
    }

    #[test]
    fn systemd_query_reports_disabled_and_stopped() {
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.home()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.home()), b"[Unit]").unwrap();

        let state = query_on(&mut host, ServiceManager::Systemd);

        assert_eq!(
            state,
            ServiceState {
                installed: true,
                enabled: Some(false),
                running: Some(false),
            }
        );
    }

    #[test]
    fn systemd_query_without_systemctl_leaves_state_unknown() {
        let mut host = Fake::new();
        std::fs::create_dir_all(unit_path(&host.home()).parent().unwrap()).unwrap();
        std::fs::write(unit_path(&host.home()), b"[Unit]").unwrap();
        host.script.borrow_mut().missing.insert("systemctl".into());

        let state = query_on(&mut host, ServiceManager::Systemd);

        assert_eq!(
            state,
            ServiceState {
                installed: true,
                enabled: None,
                running: None,
            }
        );
    }

    #[test]
    fn launchd_query_without_a_plist_runs_no_commands() {
        let mut host = Fake::new();

        let state = query_on(&mut host, ServiceManager::Launchd);

        assert_eq!(state, ServiceState::unknown());
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
                installed: true,
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
                installed: true,
                enabled: Some(false),
                running: None,
            }
        );
    }

    #[test]
    fn launchd_query_reads_an_exited_job_as_stopped() {
        assert_eq!(launchd_running_state("state = running\n"), Some(true));
        assert_eq!(launchd_running_state("\tstate = exited;\n"), Some(false));
        assert_eq!(
            launchd_running_state("gui/501/app.toka.ts = {\n\tstate = spawn scheduled;\n}\n"),
            Some(false)
        );
        assert_eq!(launchd_running_state("no state here\n"), None);
    }

    #[test]
    fn systemd_install_writes_enables_starts_and_lingers() {
        let mut host = Fake::new();
        let home = host.home();

        let outcome = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Install { link: false },
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(unit_path(&home)).unwrap(),
            render_unit(&host.executable)
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

        let error = perform_on(
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
            render_plist(&host.executable)
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
        assert!(
            host.changed(&outcome)
                .iter()
                .any(|action| action == "reloaded app.toka.ts"),
            "{}",
            host.changed(&outcome).join("\n")
        );
    }

    #[test]
    fn launchd_install_kickstarts_a_loaded_but_stopped_agent() {
        let mut host = Fake::new();
        std::fs::create_dir_all(plist_path(&host.home()).parent().unwrap()).unwrap();
        std::fs::write(plist_path(&host.home()), render_plist(&host.executable)).unwrap();
        host.script.borrow_mut().bootstrapped = true;
        host.script.borrow_mut().job_running = false;

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

        let error = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: false },
        )
        .unwrap_err();

        assert!(error.contains("launchd"), "{error}");
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
    fn install_with_link_refuses_a_directory_in_the_way() {
        let mut host = Fake::new();
        std::fs::create_dir_all(link_path(&host.home())).unwrap();

        let error = perform_on(
            &mut host,
            ServiceManager::Launchd,
            ServiceOp::Install { link: true },
        )
        .unwrap_err();

        assert!(error.contains("a directory is in the way"), "{error}");
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

        assert!(!unit_path(&host.home()).exists());
        for verb in ["stop toka-search", "disable toka-search", "daemon-reload"] {
            assert!(!host.calls_to(verb).is_empty(), "{verb}: {:?}", host.calls);
        }
        assert_eq!(host.changed(&outcome).len(), 4);
        assert!(host.unchanged(&outcome).is_empty());
    }

    #[test]
    fn systemd_uninstall_is_idempotent_when_absent() {
        let mut host = Fake::new();

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

        let error = perform_on(
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
            unit_path(&host.home()).exists(),
            "a failed stop removes nothing"
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
        host.purge = vec![index.clone(), logs.clone(), cache.clone()];

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
        host.purge = vec![locked.clone()];

        let error = perform_on(
            &mut host,
            ServiceManager::Systemd,
            ServiceOp::Uninstall { purge_data: true },
        )
        .unwrap_err();

        assert!(error.contains("could not remove"), "{error}");
        assert!(locked.join("index.sqlite3").is_file());
        // Let the tempdir cleanup remove what the purge could not.
        let mut permissions = std::fs::metadata(&locked).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&locked, permissions).unwrap();
    }

    #[test]
    fn systemd_start_stop_and_restart_converge() {
        let mut host = Fake::new();

        let error = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Start).unwrap_err();
        assert!(error.contains("not installed"), "{error}");
        let error = perform_on(&mut host, ServiceManager::Systemd, ServiceOp::Restart).unwrap_err();
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

        let error = perform_on(&mut host, ServiceManager::Launchd, ServiceOp::Start).unwrap_err();
        assert!(error.contains("not installed"), "{error}");
        let error = perform_on(&mut host, ServiceManager::Launchd, ServiceOp::Restart).unwrap_err();
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
                "systemctl --user is-active --quiet toka-search.service".to_owned(),
                "systemctl --user stop toka-search.service".to_owned(),
                "systemctl --user is-active --quiet toka-indexer.service".to_owned()
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
}
