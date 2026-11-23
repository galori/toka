//! The indexer daemon's published liveness, read by `ts status`.
//!
//! The daemon rewrites the single `daemon_state` row every [`HEARTBEAT_INTERVAL`]
//! and after every reconcile, so a reader can tell at a glance whether the
//! daemon is alive and what it is doing without asking systemd or launchd.
//! Liveness needs both a fresh heartbeat and a live pid: either alone could
//! be a crashed daemon or a recycled pid.

use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// How often the daemon republishes its row while waiting between passes.
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

/// A heartbeat older than this means the daemon is gone: liveness needs a
/// heartbeat less than ten seconds old AND a live pid.
const HEARTBEAT_FRESH_FOR: Duration = Duration::from_secs(10);

/// What the daemon is doing right now. Stored verbatim in `daemon_state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DaemonActivity {
    Idle,
    Scanning,
    Repairing,
    Thumbnails,
    OfflineWaiting,
    /// An activity this `ts` does not know: a newer daemon's heartbeat.
    /// Reading it never fails — old `ts` shows `unknown` and exits 0.
    Unknown,
}

impl DaemonActivity {
    pub(crate) fn from_database(value: &str) -> Self {
        match value {
            "idle" => Self::Idle,
            "scanning" => Self::Scanning,
            "repairing" => Self::Repairing,
            "thumbnails" => Self::Thumbnails,
            "offline-waiting" => Self::OfflineWaiting,
            _ => Self::Unknown,
        }
    }

    pub(crate) fn as_database(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Scanning => "scanning",
            Self::Repairing => "repairing",
            Self::Thumbnails => "thumbnails",
            Self::OfflineWaiting => "offline-waiting",
            // A read-only fallback, never published: the daemon only
            // writes the concrete activities above, and the table's CHECK
            // would reject anything else.
            Self::Unknown => "unknown",
        }
    }

    /// The human label, kept identical to the stored value so logs, the
    /// database, and `ts status` never disagree about an activity's name.
    fn label(self) -> &'static str {
        self.as_database()
    }
}

/// Work still queued, as the daemon last published it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonQueue {
    /// Roots with work the daemon can act on: pending, scanning, or
    /// errored (including ones backing off before a retry). Offline
    /// roots are not queued work and count separately as
    /// [`DaemonStatus::offline_roots`].
    pub pending_roots: u64,
    /// Indexable files seen but not yet committed by scans in progress:
    /// a lower bound, since a scan cannot know how many files it has not
    /// reached yet. Unsupported, empty, and unreadable files never count.
    pub files_remaining: u64,
    pub pending_repairs: u64,
    /// Folders still awaiting thumbnail generation, counted per folder:
    /// one folder's videos generate as a batch, so this answers "how much
    /// thumbnail work remains" rather than an exact file count.
    pub pending_thumbnails: u64,
}

impl DaemonQueue {
    /// Whether no actionable work is queued. Offline roots are not
    /// queued work and never affect this: `ts status --watch` exits
    /// with offline folders outstanding.
    pub fn is_empty(&self) -> bool {
        self.pending_roots == 0
            && self.files_remaining == 0
            && self.pending_repairs == 0
            && self.pending_thumbnails == 0
    }
}

/// What `ts status` reports: whether the daemon is alive, what it is doing,
/// and what work remains. `pid`, `uptime_secs`, and `heartbeat_age_secs` are
/// filled whenever the daemon ever published a row, even a stale one, so a
/// dead daemon still reports when it was last seen; `activity`, the current
/// paths, and the queue only describe a live daemon and are empty otherwise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
    pub heartbeat_age_secs: Option<u64>,
    pub activity: Option<DaemonActivity>,
    // Lossy on purpose, like the stats report: real paths can hold
    // non-UTF-8 bytes on Unix, and those must not break this document.
    #[serde(serialize_with = "crate::lossy_path::serialize_option")]
    pub current_root: Option<PathBuf>,
    #[serde(serialize_with = "crate::lossy_path::serialize_option")]
    pub current_path: Option<PathBuf>,
    pub queue: DaemonQueue,
    /// Roots whose storage is missing, as last recorded. Read live rather
    /// than published with the heartbeat, and never queued work: `ts
    /// status --watch` exits with offline folders outstanding.
    pub offline_roots: u64,
    /// Whether the indexer service is running. `None` where no service
    /// manager reports it (today: everywhere but Linux).
    pub service_running: Option<bool>,
}

/// One heartbeat's activity snapshot: everything a beat reports except
/// the daemon's own identity (pid and start time travel separately, so a
/// beat stays a plain snapshot of the moment).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DaemonBeat {
    pub heartbeat_at_ns: i64,
    pub activity: DaemonActivity,
    pub current_root: Option<PathBuf>,
    pub current_path: Option<PathBuf>,
    pub queue: DaemonQueue,
}

/// The published row, before liveness is judged.
pub(crate) struct DaemonStateRow {
    pub pid: Option<u32>,
    pub started_at_ns: i64,
    pub heartbeat_at_ns: i64,
    pub activity: DaemonActivity,
    pub current_root: Option<PathBuf>,
    pub current_path: Option<PathBuf>,
    pub queue: DaemonQueue,
}

/// Reads the daemon's status without changing the index: the index opens
/// read-only, so a version 1 database is never migrated and stays usable
/// by a version 1 app, and a missing index reports a stopped daemon
/// instead of being created. (SQLite may still touch `-wal`/`-shm`
/// sidecars beside the database while reading.)
pub fn read_status(database: &Path) -> Result<DaemonStatus, String> {
    // systemd only exists on Linux; elsewhere there is no service to ask.
    #[cfg(any(target_os = "linux", test))]
    let service_running = crate::indexer_daemon::indexer_running();
    #[cfg(not(any(target_os = "linux", test)))]
    let service_running: Option<bool> = None;
    let stopped = |offline_roots: u64| DaemonStatus {
        running: false,
        pid: None,
        uptime_secs: None,
        heartbeat_age_secs: None,
        activity: None,
        current_root: None,
        current_path: None,
        queue: DaemonQueue::default(),
        offline_roots,
        service_running,
    };
    // A missing index is a stopped daemon, not an error; anything else
    // uses the shared read taxonomy so the exit codes cannot drift.
    if !crate::owned_index::index_file_for_inspection(database)? {
        return Ok(stopped(0));
    }
    let index = crate::owned_index::OwnedIndex::open_read_only(database)
        .map_err(|error| error.to_string())?;
    let offline_roots = index
        .offline_root_count()
        .map_err(|error| error.to_string())?;
    let Some(row) = index
        .read_daemon_state()
        .map_err(|error| error.to_string())?
    else {
        return Ok(stopped(offline_roots));
    };
    let now_ns = crate::timestamp::now_ns();
    let heartbeat_age_secs = elapsed_secs(row.heartbeat_at_ns, now_ns);
    let fresh = Duration::from_secs(heartbeat_age_secs) < HEARTBEAT_FRESH_FOR;
    let alive = row.pid.is_some_and(crate::search_log::process_is_alive);
    if !fresh || !alive {
        return Ok(DaemonStatus {
            running: false,
            pid: row.pid,
            uptime_secs: Some(elapsed_secs(row.started_at_ns, now_ns)),
            heartbeat_age_secs: Some(heartbeat_age_secs),
            activity: None,
            current_root: None,
            current_path: None,
            queue: DaemonQueue::default(),
            offline_roots,
            service_running,
        });
    }
    Ok(DaemonStatus {
        running: true,
        pid: row.pid,
        uptime_secs: Some(elapsed_secs(row.started_at_ns, now_ns)),
        heartbeat_age_secs: Some(heartbeat_age_secs),
        activity: Some(row.activity),
        current_root: row.current_root,
        current_path: row.current_path,
        queue: row.queue,
        offline_roots,
        service_running,
    })
}

/// Whole seconds from `earlier_ns` to `now_ns`. A timestamp in the future —
/// a clock step between the daemon's write and this read — reads as zero,
/// never negative. (`saturating_sub` alone is not enough: it clamps at
/// `i64::MIN`, so a future timestamp would still fail the `u64` conversion.)
fn elapsed_secs(earlier_ns: i64, now_ns: i64) -> u64 {
    u64::try_from(now_ns.saturating_sub(earlier_ns).max(0) / 1_000_000_000).unwrap_or(u64::MAX)
}

pub(crate) fn format_duration(total_secs: u64) -> String {
    if total_secs < 60 {
        return format!("{total_secs}s");
    }
    let minutes = total_secs / 60;
    if minutes < 60 {
        let seconds = total_secs % 60;
        if seconds == 0 {
            return format!("{minutes}m");
        }
        return format!("{minutes}m {seconds}s");
    }
    let hours = minutes / 60;
    if hours < 24 {
        let minutes = minutes % 60;
        if minutes == 0 {
            return format!("{hours}h");
        }
        return format!("{hours}h {minutes}m");
    }
    let days = hours / 24;
    let hours = hours % 24;
    if hours == 0 {
        return format!("{days}d");
    }
    format!("{days}d {hours}h")
}

pub fn render(status: &DaemonStatus) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if !status.running {
        let mut line = "Daemon:   not running".to_owned();
        if let Some(age) = status.heartbeat_age_secs {
            let _ = write!(line, " (last heartbeat {} ago", format_duration(age));
            if let Some(pid) = status.pid {
                let _ = write!(line, ", pid {pid}");
            }
            line.push(')');
        }
        let _ = writeln!(out, "{line}");
        render_offline_line(&mut out, status.offline_roots);
        match status.service_running {
            Some(true) => {
                let _ = writeln!(out, "Service:  running");
            }
            Some(false) => {
                let _ = writeln!(out, "Service:  not running");
            }
            None => {}
        }
        return out;
    }
    let mut line = "Daemon:   running".to_owned();
    if let Some(pid) = status.pid {
        let _ = write!(line, " (pid {pid}");
        if let Some(uptime) = status.uptime_secs {
            let _ = write!(line, ", uptime {}", format_duration(uptime));
        }
        if let Some(age) = status.heartbeat_age_secs {
            let _ = write!(line, ", heartbeat {} ago", format_duration(age));
        }
        line.push(')');
    }
    let _ = writeln!(out, "{line}");
    if let Some(activity) = status.activity {
        let _ = writeln!(out, "Activity: {}", activity.label());
    }
    if let Some(root) = &status.current_root {
        let _ = writeln!(out, "Folder:   {}", root.display());
    }
    if let Some(path) = &status.current_path {
        let _ = writeln!(out, "Current:  {}", path.display());
    }
    if !status.queue.is_empty() {
        let mut parts = Vec::new();
        if status.queue.pending_roots > 0 {
            parts.push(format!(
                "{} folder{}",
                status.queue.pending_roots,
                if status.queue.pending_roots == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if status.queue.files_remaining > 0 {
            parts.push(format!("{} files", status.queue.files_remaining));
        }
        if status.queue.pending_repairs > 0 {
            parts.push(format!(
                "{} repair{}",
                status.queue.pending_repairs,
                if status.queue.pending_repairs == 1 {
                    ""
                } else {
                    "s"
                }
            ));
        }
        if status.queue.pending_thumbnails > 0 {
            parts.push(format!("{} thumbnails", status.queue.pending_thumbnails));
        }
        let _ = writeln!(out, "Queue:    {} still to index", parts.join(" / "));
    }
    render_offline_line(&mut out, status.offline_roots);
    match status.service_running {
        Some(true) => {
            let _ = writeln!(out, "Service:  running");
        }
        Some(false) => {
            let _ = writeln!(out, "Service:  not running");
        }
        None => {}
    }
    out
}

/// The offline line, shared by the running and stopped reports: folders
/// whose storage is missing wait for their disk, not for the daemon, so
/// they are listed apart from the queue and never hold `--watch`.
fn render_offline_line(out: &mut String, offline_roots: u64) {
    use std::fmt::Write as _;
    if offline_roots > 0 {
        let _ = writeln!(
            out,
            "Offline:  {offline_roots} folder{} waiting for storage",
            if offline_roots == 1 { "" } else { "s" },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::owned_index::OwnedIndex;
    use tempfile::tempdir;

    const SECOND_NS: i64 = 1_000_000_000;

    fn open_database() -> (tempfile::TempDir, PathBuf) {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        OwnedIndex::open(&database).unwrap();
        (directory, database)
    }

    fn publish(
        database: &Path,
        pid: u32,
        started_ago_secs: i64,
        heartbeat_ago_secs: i64,
        activity: DaemonActivity,
    ) {
        let now = crate::timestamp::now_ns();
        let beat = DaemonBeat {
            heartbeat_at_ns: now - heartbeat_ago_secs * SECOND_NS,
            activity,
            current_root: Some(PathBuf::from("/media")),
            current_path: Some(PathBuf::from("/media/a.mp4")),
            queue: DaemonQueue {
                pending_roots: 1,
                files_remaining: 12,
                pending_repairs: 0,
                pending_thumbnails: 3,
            },
        };
        OwnedIndex::open(database)
            .unwrap()
            .publish_daemon_state(pid, now - started_ago_secs * SECOND_NS, &beat)
            .unwrap();
    }

    fn running_status() -> DaemonStatus {
        DaemonStatus {
            running: true,
            pid: Some(1234),
            uptime_secs: Some(65),
            heartbeat_age_secs: Some(1),
            activity: Some(DaemonActivity::Scanning),
            current_root: Some(PathBuf::from("/media")),
            current_path: Some(PathBuf::from("/media/a.mp4")),
            queue: DaemonQueue {
                pending_roots: 2,
                files_remaining: 15,
                pending_repairs: 0,
                pending_thumbnails: 3,
            },
            offline_roots: 0,
            service_running: Some(true),
        }
    }

    #[test]
    fn activities_round_trip_through_the_database() {
        for (activity, stored) in [
            (DaemonActivity::Idle, "idle"),
            (DaemonActivity::Scanning, "scanning"),
            (DaemonActivity::Repairing, "repairing"),
            (DaemonActivity::Thumbnails, "thumbnails"),
            (DaemonActivity::OfflineWaiting, "offline-waiting"),
            (DaemonActivity::Unknown, "unknown"),
        ] {
            assert_eq!(activity.as_database(), stored);
            assert_eq!(DaemonActivity::from_database(stored), activity);
            assert_eq!(activity.label(), stored);
        }
        // A newer daemon's activity reads as unknown instead of failing,
        // so old `ts` keeps reporting against a new daemon.
        assert_eq!(
            DaemonActivity::from_database("compacting"),
            DaemonActivity::Unknown
        );
    }

    #[test]
    fn status_without_an_index_reports_a_stopped_daemon() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");

        let status = read_status(&database).unwrap();

        assert!(!status.running);
        assert_eq!(status.pid, None);
        assert_eq!(status.uptime_secs, None);
        assert_eq!(status.heartbeat_age_secs, None);
        assert_eq!(status.activity, None);
        assert!(status.queue.is_empty());
        assert!(!database.is_file(), "status must not create an index");
    }

    #[test]
    #[cfg(unix)]
    fn status_on_a_metadata_failure_errors_instead_of_reporting_stopped() {
        let directory = tempdir().unwrap();
        // A component past NAME_MAX fails metadata with ENAMETOOLONG on
        // Unix for every user — unlike chmod tricks, which root sails
        // through.
        let database = directory.path().join("a".repeat(300)).join("index.sqlite3");

        let error = read_status(&database).unwrap_err();

        assert!(
            error.contains("could not read its search index"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn status_on_a_directory_at_the_index_path_errors_instead_of_reporting_stopped() {
        let directory = tempdir().unwrap();

        let error = read_status(directory.path()).unwrap_err();

        assert!(error.contains("is not a file"), "unexpected error: {error}");
    }

    #[test]
    fn status_without_a_heartbeat_row_reports_a_stopped_daemon() {
        let (_directory, database) = open_database();

        let status = read_status(&database).unwrap();

        assert!(!status.running);
        assert_eq!(status.pid, None);
        assert_eq!(status.activity, None);
    }

    #[test]
    fn status_on_a_version_1_database_reports_stopped_without_migrating() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        OwnedIndex::open(&database).unwrap();
        {
            let connection = rusqlite::Connection::open(&database).unwrap();
            // Rewind to version 1: no liveness table, no indexable
            // counter, version markers back to 1.
            connection
                .execute_batch(
                    "DROP TABLE daemon_state;
                     ALTER TABLE scan_runs DROP COLUMN files_indexable;
                     UPDATE schema_meta SET value = 1 WHERE key = 'schema_version';",
                )
                .unwrap();
            connection.pragma_update(None, "user_version", 1).unwrap();
        }
        let before = std::fs::read(&database).unwrap();

        let status = read_status(&database).unwrap();

        assert!(!status.running);
        assert_eq!(status.pid, None);
        assert_eq!(status.activity, None);
        // Still version 1, byte for byte: status must never migrate.
        let version: i64 = rusqlite::Connection::open(&database)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        assert_eq!(std::fs::read(&database).unwrap(), before);
    }

    #[test]
    fn status_reports_the_indexer_service_state() {
        let (_directory, database) = open_database();

        assert_eq!(
            read_status(&database).unwrap().service_running,
            crate::indexer_daemon::indexer_running()
        );
    }

    #[test]
    fn fresh_heartbeat_with_a_live_pid_reports_running() {
        let (_directory, database) = open_database();
        publish(
            &database,
            std::process::id(),
            65,
            1,
            DaemonActivity::Scanning,
        );

        let status = read_status(&database).unwrap();

        assert!(status.running);
        assert_eq!(status.pid, Some(std::process::id()));
        assert!((63..=67).contains(&status.uptime_secs.unwrap()));
        assert!(status.heartbeat_age_secs.unwrap() <= 2);
        assert_eq!(status.activity, Some(DaemonActivity::Scanning));
        assert_eq!(status.current_root, Some(PathBuf::from("/media")));
        assert_eq!(status.current_path, Some(PathBuf::from("/media/a.mp4")));
        assert_eq!(status.queue.pending_roots, 1);
        assert_eq!(status.queue.files_remaining, 12);
        assert_eq!(status.queue.pending_thumbnails, 3);
    }

    #[test]
    fn stale_heartbeat_reports_stopped_but_remembers_the_last_beat() {
        let (_directory, database) = open_database();
        publish(
            &database,
            std::process::id(),
            90,
            30,
            DaemonActivity::Scanning,
        );

        let status = read_status(&database).unwrap();

        assert!(!status.running);
        assert_eq!(status.pid, Some(std::process::id()));
        assert!((28..=32).contains(&status.heartbeat_age_secs.unwrap()));
        assert_eq!(status.activity, None);
        assert_eq!(status.current_root, None);
        assert!(status.queue.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn fresh_heartbeat_with_a_dead_pid_reports_stopped() {
        let (_directory, database) = open_database();
        // No real process holds this pid on any test machine, and it
        // survives the pid_t conversion without wrapping to a group id.
        let dead = u32::try_from(i32::MAX).unwrap();
        publish(&database, dead, 65, 1, DaemonActivity::Scanning);

        let status = read_status(&database).unwrap();

        assert!(!status.running);
        assert_eq!(status.pid, Some(dead));
    }

    #[test]
    fn heartbeat_from_the_future_reads_as_fresh() {
        let (_directory, database) = open_database();
        publish(
            &database,
            std::process::id(),
            65,
            -5,
            DaemonActivity::Scanning,
        );

        let status = read_status(&database).unwrap();

        assert!(status.running);
        assert_eq!(status.heartbeat_age_secs, Some(0));
    }

    #[test]
    fn status_serializes_to_the_json_contract() {
        let json = serde_json::to_value(running_status()).unwrap();

        assert_eq!(json["running"], true);
        assert_eq!(json["pid"], 1234);
        assert_eq!(json["uptimeSecs"], 65);
        assert_eq!(json["heartbeatAgeSecs"], 1);
        assert_eq!(json["activity"], "scanning");
        assert_eq!(json["currentRoot"], "/media");
        assert_eq!(json["currentPath"], "/media/a.mp4");
        assert_eq!(json["queue"]["pendingRoots"], 2);
        assert_eq!(json["queue"]["filesRemaining"], 15);
        assert_eq!(json["queue"]["pendingRepairs"], 0);
        assert_eq!(json["queue"]["pendingThumbnails"], 3);
        assert_eq!(json["offlineRoots"], 0);
        assert_eq!(json["serviceRunning"], true);
    }

    #[test]
    fn heartbeat_freshness_tolerates_several_missed_beats() {
        // Liveness must not flap when one beat publishes late: the
        // freshness window spans several publish intervals, so a missed
        // beat plus scheduling jitter still reads as running.
        assert!(
            HEARTBEAT_FRESH_FOR >= 3 * HEARTBEAT_INTERVAL,
            "fresh window {HEARTBEAT_FRESH_FOR:?} must span several {HEARTBEAT_INTERVAL:?} beats"
        );
    }

    #[test]
    fn unknown_activity_from_a_newer_daemon_reports_running() {
        let directory = tempdir().unwrap();
        let database = directory.path().join("index.sqlite3");
        {
            let connection = rusqlite::Connection::open(&database).unwrap();
            // A future daemon_state row: no activity CHECK, holding an
            // activity this `ts` never learned.
            connection
                .execute_batch(
                    "CREATE TABLE daemon_state (
                        id INTEGER PRIMARY KEY,
                        pid INTEGER NOT NULL,
                        started_at_ns INTEGER NOT NULL,
                        heartbeat_at_ns INTEGER NOT NULL,
                        activity TEXT NOT NULL,
                        current_root BLOB,
                        current_path BLOB,
                        pending_roots INTEGER NOT NULL DEFAULT 0,
                        files_remaining INTEGER NOT NULL DEFAULT 0,
                        pending_repairs INTEGER NOT NULL DEFAULT 0,
                        pending_thumbnails INTEGER NOT NULL DEFAULT 0
                    );",
                )
                .unwrap();
            let now = crate::timestamp::now_ns();
            connection
                .execute(
                    "INSERT INTO daemon_state (
                        id, pid, started_at_ns, heartbeat_at_ns, activity,
                        current_root, current_path,
                        pending_roots, files_remaining, pending_repairs, pending_thumbnails
                     ) VALUES (1, ?1, ?2, ?3, 'frobnicate', NULL, NULL, 0, 0, 0, 0)",
                    rusqlite::params![std::process::id(), now - 65 * SECOND_NS, now - SECOND_NS,],
                )
                .unwrap();
            connection.pragma_update(None, "user_version", 2).unwrap();
        }

        let status = read_status(&database).unwrap();

        // Old `ts` against a new daemon: running, activity unknown, and
        // the report still renders — the command exits 0.
        assert!(status.running);
        assert_eq!(status.activity, Some(DaemonActivity::Unknown));
        let text = render(&status);
        assert!(text.contains("Activity: unknown\n"), "{text}");
    }

    #[test]
    fn offline_folders_render_on_their_own_line() {
        let mut status = running_status();
        status.offline_roots = 2;

        let text = render(&status);

        assert!(
            text.contains("Queue:    2 folders / 15 files / 3 thumbnails still to index\n"),
            "{text}"
        );
        assert!(
            text.contains("Offline:  2 folders waiting for storage\n"),
            "{text}"
        );

        let mut status = running_status();
        status.activity = Some(DaemonActivity::Idle);
        status.current_root = None;
        status.current_path = None;
        status.queue = DaemonQueue::default();
        status.offline_roots = 1;
        status.service_running = None;

        let text = render(&status);

        // Idle with nothing queued omits the queue line but still notes
        // the offline folder.
        assert!(!text.contains("Queue:"), "{text}");
        assert!(
            text.contains("Offline:  1 folder waiting for storage\n"),
            "{text}"
        );

        let mut stopped = DaemonStatus {
            running: false,
            pid: Some(1234),
            uptime_secs: Some(90),
            heartbeat_age_secs: Some(300),
            activity: None,
            current_root: None,
            current_path: None,
            queue: DaemonQueue::default(),
            offline_roots: 1,
            service_running: Some(false),
        };
        let text = render(&stopped);
        assert!(
            text.contains("Offline:  1 folder waiting for storage\n"),
            "{text}"
        );
        stopped.offline_roots = 0;
        assert!(!render(&stopped).contains("Offline:"), "{text}");
    }

    #[test]
    fn stopped_status_serializes_empty_fields_as_null() {
        let status = DaemonStatus {
            running: false,
            pid: None,
            uptime_secs: None,
            heartbeat_age_secs: None,
            activity: None,
            current_root: None,
            current_path: None,
            queue: DaemonQueue::default(),
            offline_roots: 0,
            service_running: None,
        };

        let json = serde_json::to_value(status).unwrap();

        assert_eq!(json["running"], false);
        assert!(json["pid"].is_null());
        assert!(json["activity"].is_null());
        assert!(json["currentRoot"].is_null());
        assert!(json["currentPath"].is_null());
        assert_eq!(json["queue"]["pendingRoots"], 0);
        assert!(json["serviceRunning"].is_null());
    }

    #[test]
    #[cfg(unix)]
    fn non_utf8_current_paths_serialize_lossy_instead_of_failing() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let mut status = running_status();
        status.current_path = Some(PathBuf::from(OsString::from_vec(
            b"/media/family-\xff-trip.mp4".to_vec(),
        )));

        let json = serde_json::to_value(status).unwrap();

        assert_eq!(json["currentPath"], "/media/family-�-trip.mp4");
    }

    #[test]
    fn running_scan_renders_activity_folder_current_and_queue() {
        let text = render(&running_status());

        assert!(text.contains("Daemon:   running (pid 1234"), "{text}");
        assert!(text.contains("uptime 1m 5s"), "{text}");
        assert!(text.contains("heartbeat 1s ago"), "{text}");
        assert!(text.contains("Activity: scanning\n"), "{text}");
        assert!(text.contains("Folder:   /media\n"), "{text}");
        assert!(text.contains("Current:  /media/a.mp4\n"), "{text}");
        assert!(
            text.contains("Queue:    2 folders / 15 files / 3 thumbnails still to index\n"),
            "{text}"
        );
        assert!(text.contains("Service:  running\n"), "{text}");
    }

    #[test]
    fn idle_daemon_omits_folder_current_and_queue_lines() {
        let mut status = running_status();
        status.activity = Some(DaemonActivity::Idle);
        status.current_root = None;
        status.current_path = None;
        status.queue = DaemonQueue::default();
        status.service_running = None;

        let text = render(&status);

        assert!(text.contains("Activity: idle\n"), "{text}");
        assert!(!text.contains("Folder:"), "{text}");
        assert!(!text.contains("Current:"), "{text}");
        assert!(!text.contains("Queue:"), "{text}");
        assert!(!text.contains("Service:"), "{text}");
    }

    #[test]
    fn stopped_daemon_renders_its_last_heartbeat() {
        let status = DaemonStatus {
            running: false,
            pid: Some(1234),
            uptime_secs: Some(90),
            heartbeat_age_secs: Some(300),
            activity: None,
            current_root: None,
            current_path: None,
            queue: DaemonQueue::default(),
            offline_roots: 0,
            service_running: Some(false),
        };

        let text = render(&status);

        assert!(
            text.contains("Daemon:   not running (last heartbeat 5m ago, pid 1234)\n"),
            "{text}"
        );
        assert!(text.contains("Service:  not running\n"), "{text}");
    }

    #[test]
    fn stopped_daemon_without_history_renders_plainly() {
        let status = DaemonStatus {
            running: false,
            pid: None,
            uptime_secs: None,
            heartbeat_age_secs: None,
            activity: None,
            current_root: None,
            current_path: None,
            queue: DaemonQueue::default(),
            offline_roots: 0,
            service_running: None,
        };

        assert_eq!(render(&status), "Daemon:   not running\n");
    }

    #[test]
    fn durations_render_compactly() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(5), "5s");
        assert_eq!(format_duration(60), "1m");
        assert_eq!(format_duration(65), "1m 5s");
        assert_eq!(format_duration(3600), "1h");
        assert_eq!(format_duration(3723), "1h 2m");
        assert_eq!(format_duration(86400), "1d");
        assert_eq!(format_duration(90000), "1d 1h");
    }
}
