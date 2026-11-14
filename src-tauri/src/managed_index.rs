use crate::search::{is_non_empty_file, is_supported_image, is_supported_video};
use crate::thumbnails;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(not(feature = "e2e"))]
use std::process::ExitStatus;

#[derive(Clone, Debug)]
pub struct IndexPaths {
    root: PathBuf,
    config: PathBuf,
    status: PathBuf,
    wake: PathBuf,
    databases: PathBuf,
    index_log: PathBuf,
}

impl IndexPaths {
    pub fn system() -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "Toka could not find your home folder.".to_owned())?;
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));
        Ok(Self {
            root: config_home.join("toka"),
            config: config_home.join("toka/index-folders.json"),
            status: data_home.join("toka/index-status.json"),
            wake: config_home.join("toka/indexer-wake"),
            databases: data_home.join("toka/indexes"),
            index_log: data_home.join("app.toka.desktop/logs/indexer.log"),
        })
    }

    #[cfg(test)]
    fn under(root: PathBuf) -> Self {
        Self {
            config: root.join("index-folders.json"),
            status: root.join("index-status.json"),
            wake: root.join("indexer-wake"),
            databases: root.join("indexes"),
            index_log: root.join("indexer.log"),
            root,
        }
    }

    /// The folders holding the search folder list, index databases, status and
    /// indexer log: all the indexer's state, none of the user's media.
    pub fn reset_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![
            self.root.clone(),
            self.databases.clone(),
            self.index_log.clone(),
        ];
        paths.extend(self.status.parent().map(Path::to_path_buf));
        paths
    }

    pub fn database(&self, id: &str) -> PathBuf {
        self.databases.join(format!("{id}.db"))
    }
}

struct IndexLogger {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl IndexLogger {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
        }
    }

    fn record(&self, message: impl AsRef<str>) {
        let Some(parent) = self.path.parent() else {
            return;
        };
        let _guard = self.write_lock.lock().unwrap();
        if let Err(error) = fs::create_dir_all(parent) {
            eprintln!("Could not create Toka indexer log directory: {error}");
            return;
        }
        let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            eprintln!("Could not open Toka indexer log: {}", self.path.display());
            return;
        };
        if let Err(error) = writeln!(file, "[{}] {}", now_millis(), message.as_ref()) {
            eprintln!("Could not write Toka indexer log: {error}");
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ConfiguredFolder {
    id: String,
    path: PathBuf,
    mount: Option<MountIdentity>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct IndexConfig {
    #[serde(default)]
    folders: Vec<ConfiguredFolder>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct MountIdentity {
    target: PathBuf,
    source: String,
    uuid: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FolderStatus {
    #[default]
    Pending,
    Indexing,
    Ready,
    Offline,
    Error,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PersistedFolderStatus {
    status: FolderStatus,
    message: Option<String>,
    #[serde(default)]
    scanned_files: u64,
    #[serde(default)]
    scanned_videos: u64,
    #[serde(default)]
    scanned_images: u64,
    #[serde(default)]
    current_path: Option<PathBuf>,
    /// True while the scan is building this folder's first database, so its
    /// totals are not yet part of the persisted `counts`.
    #[serde(default)]
    first_scan: bool,
}

/// What the indexer has seen so far during one folder scan.
#[derive(Clone, Debug, Default)]
struct ScanProgress {
    files: u64,
    videos: u64,
    images: u64,
    current: Option<PathBuf>,
    first_scan: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct PersistedStatus {
    revision: u64,
    #[serde(default)]
    folders: HashMap<String, PersistedFolderStatus>,
    #[serde(default)]
    counts: Option<MediaCounts>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct MediaCounts {
    videos: u64,
    images: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexFolder {
    pub id: String,
    pub path: PathBuf,
    pub status: FolderStatus,
    pub message: Option<String>,
    pub scanned_files: u64,
    pub current_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexState {
    pub supported: bool,
    pub revision: u64,
    pub indexed_videos: u64,
    pub indexed_images: u64,
    pub indexing_files: u64,
    pub folders: Vec<IndexFolder>,
}

#[derive(Clone)]
pub struct IndexManager {
    paths: IndexPaths,
    legacy_counts: Arc<Mutex<Option<MediaCounts>>>,
    logger: Arc<IndexLogger>,
}

impl IndexManager {
    pub fn system() -> Result<Self, String> {
        Ok(Self::new(IndexPaths::system()?))
    }

    fn new(paths: IndexPaths) -> Self {
        Self {
            logger: Arc::new(IndexLogger::new(paths.index_log.clone())),
            paths,
            legacy_counts: Arc::new(Mutex::new(None)),
        }
    }

    pub fn state(&self) -> Result<IndexState, String> {
        let config = load_json::<IndexConfig>(&self.paths.config)?.unwrap_or_default();
        let status = load_json::<PersistedStatus>(&self.paths.status)?.unwrap_or_default();
        let indexing_files = status
            .folders
            .values()
            .filter(|entry| entry.status == FolderStatus::Indexing)
            .map(|entry| entry.scanned_files)
            .sum();
        let mut counts = status.counts.unwrap_or_else(|| {
            let mut cached = self.legacy_counts.lock().unwrap();
            *cached.get_or_insert_with(|| indexed_media_counts(&self.paths, &config.folders))
        });
        // A folder's first scan is not in `counts` until it is Ready, so show
        // what the scan has found so far. Rescans of folders that already have
        // a database are in `counts` already.
        for folder in &config.folders {
            if let Some(entry) = status
                .folders
                .get(&folder.id)
                .filter(|entry| entry.status == FolderStatus::Indexing && entry.first_scan)
            {
                counts.videos = counts.videos.saturating_add(entry.scanned_videos);
                counts.images = counts.images.saturating_add(entry.scanned_images);
            }
        }
        let folders = config
            .folders
            .into_iter()
            .map(|folder| {
                let persisted = status.folders.get(&folder.id);
                let available = folder_available(&folder);
                let folder_status = if !available {
                    FolderStatus::Offline
                } else {
                    persisted.map(|entry| entry.status).unwrap_or_else(|| {
                        if self.paths.database(&folder.id).is_file() {
                            FolderStatus::Ready
                        } else {
                            FolderStatus::Pending
                        }
                    })
                };
                IndexFolder {
                    id: folder.id,
                    path: folder.path,
                    status: folder_status,
                    message: persisted.and_then(|entry| entry.message.clone()),
                    scanned_files: persisted
                        .map(|entry| entry.scanned_files)
                        .unwrap_or_default(),
                    current_path: persisted
                        .filter(|_| folder_status == FolderStatus::Indexing)
                        .and_then(|entry| entry.current_path.clone()),
                }
            })
            .collect();
        Ok(IndexState {
            supported: true,
            revision: status.revision,
            indexed_videos: counts.videos,
            indexed_images: counts.images,
            indexing_files,
            folders,
        })
    }

    /// Everything the indexer knows, for `toka --index-stats`. Per-folder
    /// media counts walk each folder the way the app's own totals do.
    pub fn stats(&self) -> Result<crate::index_stats::IndexStats, String> {
        use crate::index_stats::{FolderState, FolderStats, IndexStats};
        let state = self.state()?;
        let config = load_json::<IndexConfig>(&self.paths.config)?.unwrap_or_default();
        let folders = state
            .folders
            .into_iter()
            .map(|folder| {
                let counts = config
                    .folders
                    .iter()
                    .find(|configured| configured.id == folder.id)
                    .filter(|_| folder.status != FolderStatus::Offline)
                    .map(|configured| count_indexed_media(std::slice::from_ref(configured)));
                FolderStats {
                    path: folder.path,
                    state: match folder.status {
                        FolderStatus::Pending => FolderState::Pending,
                        FolderStatus::Indexing => FolderState::Indexing,
                        FolderStatus::Ready => FolderState::Ready,
                        FolderStatus::Offline => FolderState::Offline,
                        FolderStatus::Error => FolderState::Error,
                    },
                    message: folder.message,
                    scanned_files: folder.scanned_files,
                    videos: counts.map(|counts| counts.videos),
                    images: counts.map(|counts| counts.images),
                }
            })
            .collect();
        Ok(IndexStats {
            revision: state.revision,
            videos: state.indexed_videos,
            images: state.indexed_images,
            service_running: indexer_running(),
            folders,
        })
    }

    pub fn add_folder(&self, requested: &Path) -> Result<IndexState, String> {
        let path = requested
            .canonicalize()
            .map_err(|error| format!("That folder could not be opened: {error}"))?;
        if !path.is_dir() {
            return Err("Choose a folder to index.".into());
        }
        let mut config = load_json::<IndexConfig>(&self.paths.config)?.unwrap_or_default();
        if config
            .folders
            .iter()
            .any(|folder| path.starts_with(&folder.path) || folder.path.starts_with(&path))
        {
            return Err("That folder is already covered by a Toka search folder.".into());
        }
        fs::create_dir_all(&self.paths.databases)
            .map_err(|error| format!("Toka could not create its index folder: {error}"))?;
        config.folders.push(ConfiguredFolder {
            id: uuid::Uuid::new_v4().to_string(),
            mount: mount_identity(&path),
            path: path.clone(),
        });
        write_json(&self.paths.config, &config)?;
        self.logger
            .record(format!("folder added: {}", path.display()));
        self.wake_indexer()?;
        self.state()
    }

    pub fn remove_folder(&self, id: &str) -> Result<IndexState, String> {
        let mut config = load_json::<IndexConfig>(&self.paths.config)?.unwrap_or_default();
        let before = config.folders.len();
        let removed_path = config
            .folders
            .iter()
            .find(|folder| folder.id == id)
            .map(|folder| folder.path.clone());
        config.folders.retain(|folder| folder.id != id);
        if config.folders.len() == before {
            return Err("That search folder is no longer configured.".into());
        }
        write_json(&self.paths.config, &config)?;
        match fs::remove_file(self.paths.database(id)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Toka could not remove its old index: {error}")),
        }
        let _guard = STATUS_LOCK.lock().unwrap();
        let mut status = load_json::<PersistedStatus>(&self.paths.status)?.unwrap_or_default();
        status.folders.remove(id);
        status.counts = Some(indexed_media_counts(&self.paths, &config.folders));
        write_json(&self.paths.status, &status)?;
        if let Some(path) = removed_path {
            self.logger
                .record(format!("folder removed: {}", path.display()));
        }
        self.wake_indexer()?;
        self.state()
    }

    fn wake_indexer(&self) -> Result<(), String> {
        ensure_parent(&self.paths.wake)?;
        fs::write(&self.paths.wake, now_millis().to_string().as_bytes())
            .map_err(|error| format!("Toka could not notify its indexer: {error}"))
    }
}

pub fn database_paths(paths: &IndexPaths) -> Vec<PathBuf> {
    load_json::<IndexConfig>(&paths.config)
        .ok()
        .flatten()
        .unwrap_or_default()
        .folders
        .into_iter()
        .map(|folder| paths.database(&folder.id))
        .filter(|path| path.is_file())
        .collect()
}

pub fn revision(paths: &IndexPaths) -> u64 {
    load_json::<PersistedStatus>(&paths.status)
        .ok()
        .flatten()
        .unwrap_or_default()
        .revision
}

#[cfg(not(feature = "e2e"))]
const INDEXER_SERVICE: &str = "toka-indexer.service";

#[cfg(not(feature = "e2e"))]
fn ensure_indexer_with_runner<F>(mut run: F) -> Result<(), String>
where
    F: FnMut(&[&str]) -> io::Result<ExitStatus>,
{
    let arguments = ["--user", "start", INDEXER_SERVICE];
    let status = run(&arguments)
        .map_err(|error| format!("Toka could not start its indexer service: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "Toka's indexer service could not be started (systemctl exited with {status})"
        ))
    }
}

#[cfg(not(feature = "e2e"))]
pub fn ensure_indexer() {
    if let Err(message) =
        ensure_indexer_with_runner(|arguments| Command::new("systemctl").args(arguments).status())
    {
        eprintln!("{message}");
    }
}

/// Whether the indexer service is running right now, if systemd can say.
#[cfg(not(feature = "e2e"))]
fn indexer_running() -> Option<bool> {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", INDEXER_SERVICE])
        .status()
        .ok()
        .map(|status| status.success())
}

#[cfg(feature = "e2e")]
fn indexer_running() -> Option<bool> {
    None
}

/// Stops the indexer service so it cannot recreate state while it is deleted.
/// The next Toka launch starts it again.
#[cfg(not(feature = "e2e"))]
pub fn stop_indexer() -> Result<(), String> {
    let status = Command::new("systemctl")
        .args(["--user", "stop", INDEXER_SERVICE])
        .status()
        .map_err(|error| format!("could not stop the indexer service: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "the indexer service could not be stopped (systemctl exited with {status})"
        ))
    }
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Toka's index path has no parent folder.".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Toka could not create its index folder: {error}"))
}

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("Toka could not read {}: {error}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Toka could not read {}: {error}", path.display())),
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    ensure_parent(path)?;
    let temporary = path.with_extension("json.new");
    let body = serde_json::to_vec_pretty(value)
        .map_err(|error| format!("Toka could not encode its index settings: {error}"))?;
    fs::write(&temporary, body)
        .and_then(|_| fs::rename(&temporary, path))
        .map_err(|error| format!("Toka could not save {}: {error}", path.display()))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Deserialize)]
struct FindmntOutput {
    filesystems: Vec<FindmntFilesystem>,
}

#[derive(Deserialize)]
struct FindmntFilesystem {
    target: PathBuf,
    source: String,
    uuid: Option<String>,
}

fn mount_identity(path: &Path) -> Option<MountIdentity> {
    let output = Command::new("findmnt")
        .args(["--json", "--target"])
        .arg(path)
        .args(["--output", "TARGET,SOURCE,UUID"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let found: FindmntOutput = serde_json::from_slice(&output.stdout).ok()?;
    let mount = found.filesystems.into_iter().next()?;
    Some(MountIdentity {
        target: mount.target,
        source: mount.source,
        uuid: mount.uuid.filter(|uuid| !uuid.is_empty()),
    })
}

fn folder_available(folder: &ConfiguredFolder) -> bool {
    if !folder.path.is_dir() {
        return false;
    }
    let Some(expected) = &folder.mount else {
        return true;
    };
    let Some(current) = mount_identity(&folder.path) else {
        return false;
    };
    if current.target != expected.target {
        return false;
    }
    match (&expected.uuid, &current.uuid) {
        (Some(expected), Some(current)) => expected == current,
        _ => expected.source == current.source,
    }
}

fn updatedb_arguments(root: &Path, database: &Path) -> Vec<String> {
    [
        "-l".into(),
        "0".into(),
        "-U".into(),
        root.to_string_lossy().into_owned(),
        "-o".into(),
        database.to_string_lossy().into_owned(),
        "--verbose".into(),
        "--prunefs".into(),
        String::new(),
        "--prunenames".into(),
        String::new(),
        "--prunepaths".into(),
        String::new(),
        "--prune-bind-mounts".into(),
        "no".into(),
    ]
    .into()
}

fn bundled_tool(name: &str, override_variable: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(override_variable) {
        return path.into();
    }
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            for candidate in [
                parent.join("libexec").join(format!("toka-{name}")),
                parent.join(format!("toka-{name}")),
            ] {
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
    }
    name.into()
}

pub fn plocate_path() -> PathBuf {
    bundled_tool("plocate", "TOKA_PLOCATE_PATH")
}

fn updatedb_path() -> PathBuf {
    bundled_tool("updatedb", "TOKA_UPDATEDB_PATH")
}

/// Serialises read-modify-write updates of the status file within this process.
static STATUS_LOCK: Mutex<()> = Mutex::new(());

fn set_folder_status(
    paths: &IndexPaths,
    id: &str,
    folder_status: FolderStatus,
    message: Option<String>,
    advance_revision: bool,
) -> Result<(), String> {
    let _guard = STATUS_LOCK.lock().unwrap();
    let mut status = load_json::<PersistedStatus>(&paths.status)?.unwrap_or_default();
    status.folders.insert(
        id.to_owned(),
        PersistedFolderStatus {
            status: folder_status,
            message,
            ..PersistedFolderStatus::default()
        },
    );
    if advance_revision {
        status.revision = status.revision.saturating_add(1);
        let config = load_json::<IndexConfig>(&paths.config)?.unwrap_or_default();
        status.counts = Some(indexed_media_counts(paths, &config.folders));
    }
    write_json(&paths.status, &status)
}

fn set_folder_progress(
    paths: &IndexPaths,
    id: &str,
    progress: &ScanProgress,
) -> Result<(), String> {
    let _guard = STATUS_LOCK.lock().unwrap();
    let mut status = load_json::<PersistedStatus>(&paths.status)?.unwrap_or_default();
    let folder = status.folders.entry(id.to_owned()).or_default();
    folder.status = FolderStatus::Indexing;
    folder.message = None;
    folder.scanned_files = progress.files;
    folder.scanned_videos = progress.videos;
    folder.scanned_images = progress.images;
    folder.current_path = progress.current.clone();
    folder.first_scan = progress.first_scan;
    write_json(&paths.status, &status)
}

fn count_indexed_media(folders: &[ConfiguredFolder]) -> MediaCounts {
    let mut counts = MediaCounts::default();
    let mut pending = folders
        .iter()
        .map(|folder| folder.path.clone())
        .collect::<Vec<_>>();

    while let Some(directory) = pending.pop() {
        if crate::index_visibility::is_hidden(&directory).unwrap_or(true) {
            continue;
        }
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if crate::index_visibility::is_hidden(&path).unwrap_or(true) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() {
                if is_supported_video(&path) && is_non_empty_file(&path) {
                    counts.videos = counts.videos.saturating_add(1);
                }
                if is_supported_image(&path) && is_non_empty_file(&path) {
                    counts.images = counts.images.saturating_add(1);
                }
            }
        }
    }

    counts
}

fn indexed_media_counts(paths: &IndexPaths, folders: &[ConfiguredFolder]) -> MediaCounts {
    let indexed = folders
        .iter()
        .filter(|folder| paths.database(&folder.id).is_file())
        .cloned()
        .collect::<Vec<_>>();
    count_indexed_media(&indexed)
}

fn path_from_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn hash_path(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// Whether any component below `root` starts with a dot, matching the entries
/// `count_indexed_media` leaves out.
fn is_under_hidden_entry(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|relative| {
        relative
            .components()
            .any(|part| part.as_os_str().as_encoded_bytes().starts_with(b"."))
    })
}

fn run_updatedb_with_progress(
    paths: &IndexPaths,
    folder: &ConfiguredFolder,
    program: &Path,
    logger: &IndexLogger,
) -> Result<(std::process::ExitStatus, Vec<u8>), String> {
    let database = paths.database(&folder.id);
    let first_scan = !database.is_file();
    let mut child = Command::new(program)
        .args(updatedb_arguments(&folder.path, &database))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Toka's indexer could not start: {error}"))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Toka's indexer did not provide progress output.".to_owned());
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("Toka's indexer did not provide error output.".to_owned());
        }
    };
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = BufReader::new(stderr).read_to_end(&mut bytes);
        (result, bytes)
    });

    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut progress = ScanProgress {
        first_scan,
        ..ScanProgress::default()
    };
    let mut logged_directories = HashSet::new();
    let mut last_report = Instant::now() - Duration::from_secs(1);
    loop {
        line.clear();
        let bytes = match reader.read_until(b'\n', &mut line) {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.join();
                return Err(format!("Toka's indexer could not report progress: {error}"));
            }
        };
        if bytes == 0 {
            break;
        }
        progress.files = progress.files.saturating_add(1);
        let seen = path_from_bytes(line.strip_suffix(b"\n").unwrap_or(&line));
        // Live counts are approximate: they skip the per-file size check, and
        // the exact totals replace them when the folder becomes Ready.
        if !is_under_hidden_entry(&folder.path, &seen) {
            if is_supported_video(&seen) {
                progress.videos = progress.videos.saturating_add(1);
            }
            if is_supported_image(&seen) {
                progress.images = progress.images.saturating_add(1);
            }
        }
        if let Some(directory) = seen.parent() {
            if progress.current.as_deref() != Some(directory) {
                progress.current = Some(directory.to_path_buf());
                // Log each directory once, even if updatedb returns to it.
                if logged_directories.insert(hash_path(directory)) {
                    logger.record(format!(
                        "scanning {} ({} files, {} videos, {} images so far)",
                        directory.display(),
                        progress.files,
                        progress.videos,
                        progress.images
                    ));
                }
            }
        }
        if last_report.elapsed() >= Duration::from_millis(500) {
            if let Err(error) = set_folder_progress(paths, &folder.id, &progress) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stderr_reader.join();
                return Err(error);
            }
            last_report = Instant::now();
        }
    }
    // Record the final tallies the throttled reports above may have skipped.
    let _ = set_folder_progress(paths, &folder.id, &progress);
    let status = match child.wait() {
        Ok(status) => status,
        Err(error) => {
            let _ = stderr_reader.join();
            return Err(format!("Toka's indexer could not finish: {error}"));
        }
    };
    let (stderr_result, stderr) = stderr_reader
        .join()
        .map_err(|_| "Toka's indexer error output could not be read.".to_owned())?;
    stderr_result
        .map_err(|error| format!("Toka's indexer error output could not be read: {error}"))?;
    Ok((status, stderr))
}

struct ThumbnailWorker {
    sender: mpsc::Sender<PathBuf>,
}

impl ThumbnailWorker {
    fn new(logger: Arc<IndexLogger>) -> Self {
        Self::with_generator(move |folder| {
            thumbnails::generate_folder_with_failures(folder, |video, message| {
                logger.record(format!(
                    "thumbnail generation failed for {}: {message}",
                    video.display()
                ));
            });
        })
    }

    fn with_generator<F>(generator: F) -> Self
    where
        F: Fn(&Path) + Send + 'static,
    {
        let (sender, receiver): (mpsc::Sender<PathBuf>, mpsc::Receiver<PathBuf>) = mpsc::channel();
        std::thread::Builder::new()
            .name("toka-thumbnail-worker".into())
            .spawn(move || {
                while let Ok(folder) = receiver.recv() {
                    generator(&folder);
                }
            })
            .expect("Toka could not start its thumbnail worker");
        Self { sender }
    }

    fn enqueue(&self, folder: &Path) {
        let _ = self.sender.send(folder.to_path_buf());
    }
}

fn refresh_folder(
    paths: &IndexPaths,
    folder: &ConfiguredFolder,
    worker: &ThumbnailWorker,
) -> Result<(), String> {
    refresh_folder_with_program_and_worker(paths, folder, &updatedb_path(), worker)
}

#[cfg(test)]
fn refresh_folder_with_program(
    paths: &IndexPaths,
    folder: &ConfiguredFolder,
    program: &Path,
) -> Result<(), String> {
    refresh_folder_with_optional_worker(paths, folder, program, None)
}

fn refresh_folder_with_program_and_worker(
    paths: &IndexPaths,
    folder: &ConfiguredFolder,
    program: &Path,
    worker: &ThumbnailWorker,
) -> Result<(), String> {
    refresh_folder_with_optional_worker(paths, folder, program, Some(worker))
}

fn refresh_folder_with_optional_worker(
    paths: &IndexPaths,
    folder: &ConfiguredFolder,
    program: &Path,
    worker: Option<&ThumbnailWorker>,
) -> Result<(), String> {
    let logger = IndexLogger::new(paths.index_log.clone());
    if !folder_available(folder) {
        logger.record(format!("folder offline: {}", folder.path.display()));
        return set_folder_status(paths, &folder.id, FolderStatus::Offline, None, false);
    }
    fs::create_dir_all(&paths.databases)
        .map_err(|error| format!("Toka could not create its index folder: {error}"))?;
    logger.record(format!("indexing folder: {}", folder.path.display()));
    set_folder_status(paths, &folder.id, FolderStatus::Indexing, None, false)?;
    let database = paths.database(&folder.id);
    let (status, stderr) = match run_updatedb_with_progress(paths, folder, program, &logger) {
        Ok(output) => output,
        Err(error) => {
            logger.record(format!(
                "indexing failed for {}: {error}",
                folder.path.display()
            ));
            set_folder_status(
                paths,
                &folder.id,
                FolderStatus::Error,
                Some(error.clone()),
                false,
            )?;
            return Err(error);
        }
    };
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        let message = if detail.is_empty() {
            "Toka could not update this folder's index.".into()
        } else {
            detail
        };
        logger.record(format!(
            "indexing failed for {}: {message}",
            folder.path.display()
        ));
        set_folder_status(
            paths,
            &folder.id,
            FolderStatus::Error,
            Some(message.clone()),
            false,
        )?;
        return Err(message);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Toka could not protect its private index: {error}"))?;
    }
    let counts = count_indexed_media(std::slice::from_ref(folder));
    logger.record(format!(
        "indexing complete for {}: {} videos, {} images",
        folder.path.display(),
        counts.videos,
        counts.images
    ));
    set_folder_status(paths, &folder.id, FolderStatus::Ready, None, true)?;
    if let Some(worker) = worker {
        worker.enqueue(&folder.path);
    }
    Ok(())
}

fn is_path_change(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(notify::event::ModifyKind::Name(_))
    )
}

fn is_configuration_change(paths: &IndexPaths, event: &notify::Event) -> bool {
    // Opening and reading a file is reported too, and the indexer reads its
    // configuration at the start of every pass.
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    let temporary_config = paths.config.with_extension("json.new");
    event
        .paths
        .iter()
        .any(|path| path == &paths.config || path == &temporary_config || path == &paths.wake)
}

pub fn run_daemon(paths: IndexPaths) -> Result<(), String> {
    fs::create_dir_all(&paths.root)
        .map_err(|error| format!("Toka could not create its settings folder: {error}"))?;
    let logger = Arc::new(IndexLogger::new(paths.index_log.clone()));
    logger.record("indexer started");
    let (sender, receiver) = mpsc::channel();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .map_err(|error| format!("Toka could not start watching folders: {error}"))?;
    watcher
        .watch(&paths.root, RecursiveMode::NonRecursive)
        .map_err(|error| format!("Toka could not watch its settings: {error}"))?;
    let thumbnail_worker = ThumbnailWorker::new(Arc::clone(&logger));

    let mut watched = HashSet::<PathBuf>::new();
    let mut first_pass = true;
    loop {
        let config = load_json::<IndexConfig>(&paths.config)?.unwrap_or_default();
        let mut reconnected = Vec::new();
        for folder in &config.folders {
            let available = folder_available(folder);
            if available && watched.insert(folder.path.clone()) {
                if watcher
                    .watch(&folder.path, RecursiveMode::Recursive)
                    .is_err()
                {
                    watched.remove(&folder.path);
                } else if !first_pass {
                    reconnected.push(folder);
                }
            } else if !available && watched.remove(&folder.path) {
                let _ = watcher.unwatch(&folder.path);
            }
        }

        if first_pass {
            first_pass = false;
            logger.record(format!(
                "initial index pass: {} folder(s)",
                config.folders.len()
            ));
            for folder in &config.folders {
                let _ = refresh_folder(&paths, folder, &thumbnail_worker);
            }
            continue;
        }
        if !reconnected.is_empty() {
            logger.record(format!("reconnected {} folder(s)", reconnected.len()));
            for folder in reconnected {
                let _ = refresh_folder(&paths, folder, &thumbnail_worker);
            }
            continue;
        }
        let mut dirty = HashSet::<String>::new();
        let mut config_changed = false;
        match receiver.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(event)) => {
                if is_configuration_change(&paths, &event) {
                    config_changed = true;
                } else if is_path_change(&event.kind) {
                    for folder in &config.folders {
                        if event
                            .paths
                            .iter()
                            .any(|path| path.starts_with(&folder.path))
                        {
                            dirty.insert(folder.id.clone());
                        }
                    }
                }
                while let Ok(Ok(event)) = receiver.recv_timeout(Duration::from_secs(2)) {
                    if is_configuration_change(&paths, &event) {
                        config_changed = true;
                    } else if is_path_change(&event.kind) {
                        for folder in &config.folders {
                            if event
                                .paths
                                .iter()
                                .any(|path| path.starts_with(&folder.path))
                            {
                                dirty.insert(folder.id.clone());
                            }
                        }
                    }
                }
            }
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {
                for folder in &config.folders {
                    if !folder_available(folder) {
                        let _ = set_folder_status(
                            &paths,
                            &folder.id,
                            FolderStatus::Offline,
                            None,
                            false,
                        );
                    } else if !watched.contains(&folder.path) {
                        dirty.insert(folder.id.clone());
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Toka's folder watcher stopped unexpectedly.".into())
            }
        }

        if config_changed {
            logger.record("index configuration changed");
            first_pass = true;
            continue;
        }

        for folder in &config.folders {
            if dirty.contains(&folder.id) {
                logger.record(format!("folder changed: {}", folder.path.display()));
                let _ = refresh_folder(&paths, folder, &thumbnail_worker);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };
    use tempfile::tempdir;

    #[test]
    fn adding_and_removing_a_folder_owns_only_toka_configuration_and_database() {
        let root = tempdir().unwrap();
        let media = root.path().join("Media");
        fs::create_dir(&media).unwrap();
        fs::write(media.join("clip.mp4"), b"media").unwrap();
        let paths = IndexPaths::under(root.path().join("state"));
        let manager = IndexManager::new(paths.clone());

        let state = manager.add_folder(&media).unwrap();

        assert_eq!(state.folders.len(), 1);
        assert_eq!(state.folders[0].path, media.canonicalize().unwrap());
        assert_eq!(state.folders[0].status, FolderStatus::Pending);
        assert_eq!(state.indexed_videos, 0);
        assert_eq!(state.indexed_images, 0);
        let database = paths.database(&state.folders[0].id);
        fs::write(&database, b"index").unwrap();

        manager.remove_folder(&state.folders[0].id).unwrap();

        assert!(media.join("clip.mp4").is_file());
        assert!(!database.exists());
        assert!(manager.state().unwrap().folders.is_empty());
        let log = fs::read_to_string(paths.index_log).unwrap();
        assert!(log.contains("folder added:"));
        assert!(log.contains("folder removed:"));
    }

    #[test]
    fn overlapping_search_roots_are_rejected() {
        let root = tempdir().unwrap();
        let media = root.path().join("Media");
        let nested = media.join("Trips");
        fs::create_dir_all(&nested).unwrap();
        let manager = IndexManager::new(IndexPaths::under(root.path().join("state")));
        manager.add_folder(&media).unwrap();

        let error = manager.add_folder(&nested).unwrap_err();

        assert!(error.contains("already covered"), "{error}");
    }

    #[test]
    fn private_updatedb_arguments_override_system_wide_pruning() {
        let arguments = updatedb_arguments(
            std::path::Path::new("/media/My Drive/Videos"),
            std::path::Path::new("/data/toka/index.db"),
        );

        assert_eq!(
            arguments,
            [
                "-l",
                "0",
                "-U",
                "/media/My Drive/Videos",
                "-o",
                "/data/toka/index.db",
                "--verbose",
                "--prunefs",
                "",
                "--prunenames",
                "",
                "--prunepaths",
                "",
                "--prune-bind-mounts",
                "no",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn records_files_seen_while_updatedb_reports_progress() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let media = root.path().join("Media");
        fs::create_dir(&media).unwrap();
        let paths = IndexPaths::under(root.path().join("state"));
        let folder = ConfiguredFolder {
            id: "folder".into(),
            path: media,
            mount: None,
        };
        let fake_updatedb = root.path().join("updatedb");
        fs::write(&fake_updatedb, "#!/bin/sh\nprintf 'one\\none\\n'\n").unwrap();
        fs::set_permissions(&fake_updatedb, fs::Permissions::from_mode(0o700)).unwrap();

        let (status, stderr) = run_updatedb_with_progress(
            &paths,
            &folder,
            &fake_updatedb,
            &IndexLogger::new(paths.index_log.clone()),
        )
        .unwrap();

        assert!(status.success());
        assert!(stderr.is_empty());
        assert_eq!(
            load_json::<PersistedStatus>(&paths.status)
                .unwrap()
                .unwrap()
                .folders["folder"]
                .scanned_files,
            2
        );
        assert_eq!(IndexManager::new(paths).state().unwrap().indexing_files, 2);
    }

    #[cfg(unix)]
    #[test]
    fn logs_each_directory_updatedb_visits_with_running_media_counts() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let media = root.path().join("Media");
        fs::create_dir_all(media.join("a")).unwrap();
        fs::create_dir_all(media.join("b")).unwrap();
        fs::write(media.join("a/one.mp4"), b"media").unwrap();
        fs::write(media.join("a/two.mp4"), b"media").unwrap();
        fs::write(media.join("b/cover.png"), b"media").unwrap();
        let paths = IndexPaths::under(root.path().join("state"));
        let folder = ConfiguredFolder {
            id: "folder".into(),
            path: media.clone(),
            mount: None,
        };
        let fake_updatedb = root.path().join("updatedb");
        fs::write(
            &fake_updatedb,
            format!(
                "#!/bin/sh\nprintf '{m}/a/one.mp4\\n{m}/a/two.mp4\\n{m}/b/cover.png\\n'\n",
                m = media.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake_updatedb, fs::Permissions::from_mode(0o700)).unwrap();

        run_updatedb_with_progress(
            &paths,
            &folder,
            &fake_updatedb,
            &IndexLogger::new(paths.index_log.clone()),
        )
        .unwrap();

        let log = fs::read_to_string(&paths.index_log).unwrap();
        let in_a = log
            .lines()
            .find(|line| line.contains(&format!("scanning {}/a", media.display())))
            .unwrap_or_else(|| panic!("no entry for a in {log}"));
        assert!(in_a.contains("1 files, 1 videos, 0 images"), "{in_a}");
        let in_b = log
            .lines()
            .find(|line| line.contains(&format!("scanning {}/b", media.display())))
            .unwrap_or_else(|| panic!("no entry for b in {log}"));
        assert!(in_b.contains("3 files, 2 videos, 1 images"), "{in_b}");
    }

    #[test]
    fn live_scan_progress_is_reported_in_state_until_the_index_is_ready() {
        let root = tempdir().unwrap();
        let media = root.path().join("Media");
        fs::create_dir(&media).unwrap();
        let paths = IndexPaths::under(root.path().join("state"));
        let manager = IndexManager::new(paths.clone());
        let state = manager.add_folder(&media).unwrap();
        let id = state.folders[0].id.clone();

        set_folder_progress(
            &paths,
            &id,
            &ScanProgress {
                files: 5,
                videos: 3,
                images: 1,
                current: Some(media.join("Trips")),
                first_scan: true,
            },
        )
        .unwrap();

        let state = manager.state().unwrap();
        assert_eq!(state.indexed_videos, 3);
        assert_eq!(state.indexed_images, 1);
        assert_eq!(state.folders[0].current_path, Some(media.join("Trips")));

        set_folder_status(&paths, &id, FolderStatus::Ready, None, false).unwrap();
        assert_eq!(manager.state().unwrap().folders[0].current_path, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_ready_folder_is_queued_for_background_thumbnail_generation() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let media = root.path().join("Media");
        fs::create_dir(&media).unwrap();
        fs::write(media.join("clip.mp4"), b"media").unwrap();
        fs::write(media.join("cover.jpg"), b"media").unwrap();
        let paths = IndexPaths::under(root.path().join("state"));
        let folder = ConfiguredFolder {
            id: "folder".into(),
            path: media.clone(),
            mount: None,
        };
        let fake_updatedb = root.path().join("updatedb");
        fs::write(
            &fake_updatedb,
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"-o\" ]; then shift; printf indexed > \"$1\"; exit 0; fi\n  shift\ndone\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&fake_updatedb, fs::Permissions::from_mode(0o700)).unwrap();
        write_json(
            &paths.config,
            &IndexConfig {
                folders: vec![folder.clone()],
            },
        )
        .unwrap();

        let (finished, received) = mpsc::channel();
        let worker = ThumbnailWorker::with_generator(move |folder| {
            finished.send(folder.to_path_buf()).unwrap();
        });
        refresh_folder_with_program_and_worker(&paths, &folder, &fake_updatedb, &worker).unwrap();

        assert_eq!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            media
        );
        assert_eq!(
            load_json::<PersistedStatus>(&paths.status)
                .unwrap()
                .unwrap()
                .folders["folder"]
                .status,
            FolderStatus::Ready
        );
        let log = fs::read_to_string(&paths.index_log).unwrap();
        assert!(log.contains("indexing folder:"));
        assert!(log.contains("indexing complete for"));
        let state = IndexManager::new(paths).state().unwrap();
        assert_eq!(state.indexed_videos, 1);
        assert_eq!(state.indexed_images, 1);
    }

    #[test]
    fn background_thumbnail_failures_are_written_to_the_index_log() {
        let root = tempdir().unwrap();
        let media = root.path().join("Media");
        fs::create_dir(&media).unwrap();
        let video = media.join("broken.mp4");
        fs::write(&video, b"not a video").unwrap();
        let failure = thumbnails::cache_path(&video)
            .unwrap()
            .with_extension("failed");
        let _ = fs::remove_file(&failure);
        let paths = IndexPaths::under(root.path().join("state"));

        let logger = Arc::new(IndexLogger::new(paths.index_log.clone()));
        let worker = ThumbnailWorker::new(logger);
        worker.enqueue(&media);

        let expected = format!("thumbnail generation failed for {}: ", video.display());
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut log = String::new();
        while !log.contains(&expected) && Instant::now() < deadline {
            log = fs::read_to_string(&paths.index_log).unwrap_or_default();
            thread::sleep(Duration::from_millis(20));
        }

        assert!(log.contains(&expected), "{log}");
        assert!(failure.is_file());
        let _ = fs::remove_file(failure);
    }

    #[test]
    fn counts_supported_media_across_configured_folders() {
        let root = tempdir().unwrap();
        let media = root.path().join("Media");
        let nested = media.join("Nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(media.join("clip.mp4"), b"video").unwrap();
        fs::write(media.join("empty.mp4"), b"").unwrap();
        fs::write(nested.join("cover.PNG"), b"image").unwrap();
        fs::write(nested.join("empty.jpg"), b"").unwrap();
        fs::write(nested.join("notes.txt"), b"other").unwrap();
        let folders = vec![ConfiguredFolder {
            id: "media".into(),
            path: media,
            mount: None,
        }];

        assert_eq!(
            count_indexed_media(&folders),
            MediaCounts {
                videos: 1,
                images: 1,
            }
        );
    }

    #[cfg(not(feature = "e2e"))]
    #[test]
    fn starts_the_user_indexer_service_when_toka_launches() {
        let mut commands: Vec<Vec<String>> = Vec::new();

        ensure_indexer_with_runner(|arguments| {
            commands.push(
                arguments
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
            );
            std::process::Command::new("true").status()
        })
        .unwrap();

        assert_eq!(
            commands,
            vec![vec![
                "--user".to_owned(),
                "start".to_owned(),
                "toka-indexer.service".to_owned(),
            ]]
        );
    }

    fn event(kind: EventKind, path: &Path) -> notify::Event {
        notify::Event::new(kind).add_path(path.to_path_buf())
    }

    #[test]
    fn only_configuration_and_wake_events_restart_an_index_pass() {
        let root = tempdir().unwrap();
        let paths = IndexPaths::under(root.path().join("state"));
        let modified = EventKind::Modify(notify::event::ModifyKind::Any);

        assert!(is_configuration_change(
            &paths,
            &event(modified, &paths.config)
        ));
        assert!(is_configuration_change(
            &paths,
            &event(modified, &paths.wake)
        ));
        assert!(is_configuration_change(
            &paths,
            &event(modified, &paths.config.with_extension("json.new"))
        ));
        assert!(!is_configuration_change(
            &paths,
            &event(modified, &paths.root.join(".hidden-metadata"))
        ));
    }

    #[test]
    fn reading_the_configuration_does_not_restart_an_index_pass() {
        let root = tempdir().unwrap();
        let paths = IndexPaths::under(root.path().to_path_buf());
        fs::write(&paths.config, b"{}").unwrap();
        let (sender, receiver) = mpsc::channel();
        let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        })
        .unwrap();
        watcher
            .watch(&paths.root, RecursiveMode::NonRecursive)
            .unwrap();

        // The indexer reads its own configuration on every pass; that read
        // must not look like the configuration changing, or every pass starts
        // the next one.
        let _ = load_json::<IndexConfig>(&paths.config).unwrap();

        while let Ok(Ok(event)) = receiver.recv_timeout(Duration::from_millis(500)) {
            assert!(
                !is_configuration_change(&paths, &event),
                "a read was taken for a change: {event:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_disconnected_folder_keeps_its_database_and_reuses_it_on_reconnect() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let media = root.path().join("External");
        fs::create_dir(&media).unwrap();
        let paths = IndexPaths::under(root.path().join("state"));
        let manager = IndexManager::new(paths.clone());
        let state = manager.add_folder(&media).unwrap();
        let config = load_json::<IndexConfig>(&paths.config).unwrap().unwrap();
        let folder = &config.folders[0];
        let database = paths.database(&state.folders[0].id);
        fs::write(&database, b"existing incremental database").unwrap();

        fs::remove_dir(&media).unwrap();
        refresh_folder_with_program(&paths, folder, Path::new("/does/not/exist")).unwrap();

        assert_eq!(
            fs::read(&database).unwrap(),
            b"existing incremental database"
        );
        assert_eq!(
            manager.state().unwrap().folders[0].status,
            FolderStatus::Offline
        );

        fs::create_dir(&media).unwrap();
        let fake_updatedb = root.path().join("updatedb");
        fs::write(
            &fake_updatedb,
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"-o\" ]; then shift; printf refreshed > \"$1\"; exit 0; fi\n  shift\ndone\nexit 1\n",
        )
        .unwrap();
        fs::set_permissions(&fake_updatedb, fs::Permissions::from_mode(0o700)).unwrap();
        refresh_folder_with_program(&paths, folder, &fake_updatedb).unwrap();

        assert_eq!(fs::read(&database).unwrap(), b"refreshed");
        let state = manager.state().unwrap();
        assert_eq!(state.revision, 1);
        assert_eq!(state.folders[0].status, FolderStatus::Ready);
    }

    #[cfg(unix)]
    #[test]
    fn live_counts_skip_hidden_entries_and_log_each_directory_once() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().unwrap();
        let media = root.path().join("Media");
        let paths = IndexPaths::under(root.path().join("state"));
        let folder = ConfiguredFolder {
            id: "folder".into(),
            path: media.clone(),
            mount: None,
        };
        let fake_updatedb = root.path().join("updatedb");
        fs::write(
            &fake_updatedb,
            format!(
                "#!/bin/sh\nprintf '{m}/a/one.mp4\\n{m}/a/sub/two.mp4\\n{m}/a/three.mp4\\n{m}/.Trash/x.mp4\\n{m}/a/.hidden.png\\n'\n",
                m = media.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake_updatedb, fs::Permissions::from_mode(0o700)).unwrap();

        run_updatedb_with_progress(
            &paths,
            &folder,
            &fake_updatedb,
            &IndexLogger::new(paths.index_log.clone()),
        )
        .unwrap();

        let entry = &load_json::<PersistedStatus>(&paths.status)
            .unwrap()
            .unwrap()
            .folders["folder"];
        assert_eq!((entry.scanned_videos, entry.scanned_images), (3, 0));
        let log = fs::read_to_string(&paths.index_log).unwrap();
        let in_a = format!("scanning {}/a ", media.display());
        assert_eq!(log.matches(&in_a).count(), 1, "{log}");
    }

    #[test]
    fn first_scan_live_counts_persist_until_ready() {
        let root = tempdir().unwrap();
        let paths = IndexPaths::under(root.path().join("state"));
        fs::create_dir_all(&paths.databases).unwrap();
        let media = root.path().join("Media");
        fs::create_dir(&media).unwrap();
        write_json(
            &paths.config,
            &IndexConfig {
                folders: vec![ConfiguredFolder {
                    id: "f".into(),
                    path: media,
                    mount: None,
                }],
            },
        )
        .unwrap();
        // The database already exists but the folder is not Ready yet.
        fs::write(paths.database("f"), b"db").unwrap();
        let mut status = PersistedStatus {
            counts: Some(MediaCounts::default()),
            ..PersistedStatus::default()
        };
        status.folders.insert(
            "f".into(),
            PersistedFolderStatus {
                status: FolderStatus::Indexing,
                scanned_videos: 4,
                scanned_images: 2,
                first_scan: true,
                ..PersistedFolderStatus::default()
            },
        );
        write_json(&paths.status, &status).unwrap();

        let state = IndexManager::new(paths).state().unwrap();

        assert_eq!((state.indexed_videos, state.indexed_images), (4, 2));
    }
}
