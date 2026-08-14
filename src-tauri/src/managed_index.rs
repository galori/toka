use crate::search::{is_non_empty_file, is_supported_image, is_supported_video};
use crate::thumbnails;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexState {
    pub supported: bool,
    pub revision: u64,
    pub indexed_videos: u64,
    pub indexed_images: u64,
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
        let counts = status.counts.unwrap_or_else(|| {
            let mut cached = self.legacy_counts.lock().unwrap();
            *cached.get_or_insert_with(|| count_indexed_media(&config.folders))
        });
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
                }
            })
            .collect();
        Ok(IndexState {
            supported: true,
            revision: status.revision,
            indexed_videos: counts.videos,
            indexed_images: counts.images,
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
        let mut status = load_json::<PersistedStatus>(&self.paths.status)?.unwrap_or_default();
        status.folders.remove(id);
        status.counts = Some(count_indexed_media(&config.folders));
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

fn set_folder_status(
    paths: &IndexPaths,
    id: &str,
    folder_status: FolderStatus,
    message: Option<String>,
    advance_revision: bool,
) -> Result<(), String> {
    let mut status = load_json::<PersistedStatus>(&paths.status)?.unwrap_or_default();
    status.folders.insert(
        id.to_owned(),
        PersistedFolderStatus {
            status: folder_status,
            message,
        },
    );
    if advance_revision {
        status.revision = status.revision.saturating_add(1);
        let config = load_json::<IndexConfig>(&paths.config)?.unwrap_or_default();
        status.counts = Some(count_indexed_media(&config.folders));
    }
    write_json(&paths.status, &status)
}

fn count_indexed_media(folders: &[ConfiguredFolder]) -> MediaCounts {
    let mut counts = MediaCounts::default();
    let mut pending = folders
        .iter()
        .map(|folder| folder.path.clone())
        .collect::<Vec<_>>();

    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
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
    let output = match Command::new(program)
        .args(updatedb_arguments(&folder.path, &database))
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            let message = format!("Toka's indexer could not start: {error}");
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
    };
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
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
                if event.paths.iter().any(|path| path.starts_with(&paths.root)) {
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
                    if event.paths.iter().any(|path| path.starts_with(&paths.root)) {
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
}
