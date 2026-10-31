use crate::search_log::{SearchLogContext, SearchLogger};
use crate::tags;
use crate::thumbnails;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const PAGE_SIZE: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    #[default]
    #[serde(rename = "videos")]
    Videos,
    #[serde(rename = "images")]
    Images,
    #[serde(rename = "both")]
    Both,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub query: String,
    pub page: usize,
    pub page_size: usize,
    #[serde(default)]
    pub fields: SearchFields,
    #[serde(default)]
    pub media_type: MediaType,
}

/// Which part of a video a query is matched against. The three are separate
/// haystacks rather than nested ones, so switching one off cannot be undone by
/// another: the tag block belongs to the tags, the file name is read without
/// it, and the path is the folders the file sits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchFields {
    pub tags: bool,
    pub file_name: bool,
    pub path: bool,
}

impl SearchFields {
    fn any(&self) -> bool {
        self.tags || self.file_name || self.path
    }
}

/// What a search covers before the fields can be chosen: tags, the whole file
/// name, and the folders above it.
impl Default for SearchFields {
    fn default() -> Self {
        Self {
            tags: true,
            file_name: true,
            path: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoResult {
    pub id: String,
    pub file_name: String,
    pub extension: String,
    pub thumbnail_path: Option<String>,
    pub tags: Vec<String>,
    /// The tags of the folder the file sits in, read from that folder's name.
    pub folder_tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPage {
    pub query: String,
    pub page: usize,
    pub page_size: usize,
    pub total_results: usize,
    pub total_pages: usize,
    pub index_revision: u64,
    pub results: Vec<VideoResult>,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("Enter at least one search term.")]
    InvalidQuery,
    #[error("Choose at least one of tags, file name or path to search.")]
    NoSearchFields,
    #[error("The requested search page is invalid.")]
    InvalidPage,
    #[error("{0}")]
    Provider(String),
    #[error("That video is no longer available.")]
    VideoUnavailable,
}

pub trait SearchProvider: Send + Sync {
    /// Candidate videos for `query`, which the engine then filters by the
    /// fields the viewer chose. `whole_path` asks for the wider set a folder
    /// search needs: every index Toka reads answers on file names alone unless
    /// it is told otherwise, and a video in a matching folder has no reason to
    /// carry the term in its own name.
    fn candidates(
        &self,
        query: &str,
        whole_path: bool,
        context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError>;

    fn revision(&self) -> u64 {
        0
    }
}

pub struct SearchEngine {
    provider: Arc<dyn SearchProvider>,
    logger: Arc<SearchLogger>,
    result_paths: Mutex<HashMap<String, PathBuf>>,
    /// Files Toka renamed since the search provider last indexed them, keyed by
    /// the name the provider knew. An index that filters out names whose file
    /// is gone, as `plocate --existing` does, stops listing it at all. Saving
    /// these renames lets a search find the file by its current name after a
    /// restart.
    renamed_paths: Mutex<HashMap<PathBuf, PathBuf>>,
    /// Folders Toka renamed since the provider last indexed them, keyed by the
    /// name the provider knew. Tagging a folder moves every file in it.
    renamed_folders: Mutex<HashMap<PathBuf, PathBuf>>,
    rename_overlay_path: Option<PathBuf>,
    rename_overlay_write: Mutex<()>,
    /// The order the search on screen is being read in. Only the current one is
    /// kept: a new question replaces it, and there is never a second search to
    /// go back to — the frontend asks for page one and then walks forward.
    shuffle: Mutex<Option<Shuffle>>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PersistedRenameOverlay {
    #[serde(default)]
    files: Vec<PersistedRename>,
    #[serde(default)]
    folders: Vec<PersistedRename>,
}

// Keep raw Unix path bytes; real media folders can have non-UTF-8 names.
#[cfg(unix)]
type StoredPath = Vec<u8>;
#[cfg(not(unix))]
type StoredPath = PathBuf;

#[derive(Deserialize, Serialize)]
struct PersistedRename {
    indexed: StoredPath,
    current: StoredPath,
}

fn store_path(path: &Path) -> StoredPath {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_path_buf()
    }
}

fn restore_path(path: StoredPath) -> PathBuf {
    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(OsString::from_vec(path))
    }
    #[cfg(not(unix))]
    {
        path
    }
}

impl PersistedRenameOverlay {
    fn from_maps(files: &HashMap<PathBuf, PathBuf>, folders: &HashMap<PathBuf, PathBuf>) -> Self {
        fn entries(paths: &HashMap<PathBuf, PathBuf>) -> Vec<PersistedRename> {
            let mut entries = paths
                .iter()
                .map(|(indexed, current)| PersistedRename {
                    indexed: store_path(indexed),
                    current: store_path(current),
                })
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| left.indexed.cmp(&right.indexed));
            entries
        }

        Self {
            files: entries(files),
            folders: entries(folders),
        }
    }

    fn into_maps(self) -> (HashMap<PathBuf, PathBuf>, HashMap<PathBuf, PathBuf>) {
        fn paths(entries: Vec<PersistedRename>) -> HashMap<PathBuf, PathBuf> {
            entries
                .into_iter()
                .map(|entry| (restore_path(entry.indexed), restore_path(entry.current)))
                .collect()
        }

        (paths(self.files), paths(self.folders))
    }
}

fn load_rename_overlay(path: &Path) -> io::Result<PersistedRenameOverlay> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(PersistedRenameOverlay::default())
        }
        Err(error) => Err(error),
    }
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn lock_rename_overlay(path: &Path) -> io::Result<File> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "search rename state has no parent directory",
        )
    })?;
    ensure_private_directory(parent)?;
    let lock_path = path.with_extension("lock");
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&lock_path)?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
            return Err(io::Error::last_os_error());
        }
        fs::set_permissions(lock_path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn write_rename_overlay(path: &Path, overlay: &PersistedRenameOverlay) -> io::Result<()> {
    static TEMPORARY_FILES: AtomicU64 = AtomicU64::new(0);

    let contents = serde_json::to_vec(overlay)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "search rename state has no parent directory",
        )
    })?;
    ensure_private_directory(parent)?;

    let temporary = path.with_extension(format!(
        "json.tmp-{}-{}",
        std::process::id(),
        TEMPORARY_FILES.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&contents)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

/// The seed a search's order was drawn from, under the question that asked for
/// it. Pages after the first reuse the seed so they continue that order instead
/// of drawing a new one, which would repeat some videos and skip others.
struct Shuffle {
    query: String,
    fields: SearchFields,
    media_type: MediaType,
    seed: u64,
}

impl SearchEngine {
    #[cfg(test)]
    pub fn new(provider: Arc<dyn SearchProvider>) -> Self {
        Self::new_with_logger(provider, Arc::new(SearchLogger::disabled()))
    }

    #[cfg(test)]
    pub fn new_with_logger(provider: Arc<dyn SearchProvider>, logger: Arc<SearchLogger>) -> Self {
        Self::new_with_logger_and_rename_store(provider, logger, None)
    }

    pub(crate) fn new_with_logger_and_rename_store(
        provider: Arc<dyn SearchProvider>,
        logger: Arc<SearchLogger>,
        rename_overlay_path: Option<PathBuf>,
    ) -> Self {
        let (renamed_paths, renamed_folders) = match rename_overlay_path.as_deref() {
            Some(path) => match load_rename_overlay(path) {
                Ok(overlay) => overlay.into_maps(),
                Err(error) => {
                    eprintln!("Toka could not restore pending search renames: {error}");
                    PersistedRenameOverlay::default().into_maps()
                }
            },
            None => PersistedRenameOverlay::default().into_maps(),
        };

        Self {
            provider,
            logger,
            result_paths: Mutex::new(HashMap::new()),
            renamed_paths: Mutex::new(renamed_paths),
            renamed_folders: Mutex::new(renamed_folders),
            rename_overlay_path,
            rename_overlay_write: Mutex::new(()),
            shuffle: Mutex::new(None),
        }
    }

    pub fn search(&self, request: SearchRequest) -> Result<SearchPage, SearchError> {
        self.search_filtered(request, |_| true)
    }

    pub fn search_filtered(
        &self,
        request: SearchRequest,
        filter: impl Fn(&Path) -> bool,
    ) -> Result<SearchPage, SearchError> {
        let context = self.logger.context(&request, true);
        self.search_filtered_with_context(request, context, filter)
    }

    pub fn search_filtered_for_addon(
        &self,
        request: SearchRequest,
        addon_id: &str,
        addon_capture: &str,
        filter: impl Fn(&Path) -> bool,
    ) -> Result<SearchPage, SearchError> {
        let context = self
            .logger
            .context_for_addon(&request, true, addon_id, addon_capture);
        self.search_filtered_with_context(request, context, filter)
    }

    fn search_filtered_with_context(
        &self,
        request: SearchRequest,
        context: SearchLogContext,
        filter: impl Fn(&Path) -> bool,
    ) -> Result<SearchPage, SearchError> {
        let result = self.search_page(request, &context, &filter);
        if let Err(error) = &result {
            self.logger.record_error(&context, &error.to_string());
        }
        result
    }

    fn search_page(
        &self,
        request: SearchRequest,
        context: &SearchLogContext,
        filter: &dyn Fn(&Path) -> bool,
    ) -> Result<SearchPage, SearchError> {
        let (query, mut paths) = self.matching_paths(&request, context, filter)?;
        let fields = request.fields;
        let media_type = request.media_type;

        // Sorted not to be read in this order — the shuffle below undoes it —
        // but so that the shuffle has something fixed to work from. A provider
        // is free to answer the same query in a different order each time, and
        // page two has to continue the order page one started.
        paths.sort_by(|left, right| {
            let left_name = left.file_name().unwrap_or_default().to_string_lossy();
            let right_name = right.file_name().unwrap_or_default().to_string_lossy();
            left_name
                .to_lowercase()
                .cmp(&right_name.to_lowercase())
                .then_with(|| left.cmp(right))
        });
        shuffle(
            &mut paths,
            self.shuffle_seed(&query, fields, media_type, request.page),
        );

        let total_results = paths.len();
        let total_pages = total_results.div_ceil(PAGE_SIZE);
        let start = (request.page - 1).saturating_mul(PAGE_SIZE);
        let page_paths = paths
            .into_iter()
            .skip(start)
            .take(PAGE_SIZE)
            .collect::<Vec<_>>();
        self.logger.record_returned(context, &page_paths);
        let mut known_paths = self.result_paths.lock().unwrap();
        let results = page_paths
            .into_iter()
            .map(|path| {
                let result = video_result(&path);
                known_paths.insert(result.id.clone(), path);
                result
            })
            .collect();

        Ok(SearchPage {
            query: query.to_owned(),
            page: request.page,
            page_size: PAGE_SIZE,
            total_results,
            total_pages,
            index_revision: self.provider.revision(),
            results,
        })
    }

    /// Answers whether a query's match count changed without creating result
    /// ids or replacing the shuffle used by the visible result pages.
    pub fn match_count(&self, request: SearchRequest) -> Result<usize, SearchError> {
        self.match_count_filtered(request, |_| true)
    }

    pub fn match_count_filtered(
        &self,
        request: SearchRequest,
        filter: impl Fn(&Path) -> bool,
    ) -> Result<usize, SearchError> {
        let context = self.logger.context(&request, false);
        self.match_count_filtered_with_context(request, context, filter)
    }

    pub fn match_count_filtered_for_addon(
        &self,
        request: SearchRequest,
        addon_id: &str,
        addon_capture: &str,
        filter: impl Fn(&Path) -> bool,
    ) -> Result<usize, SearchError> {
        let context = self
            .logger
            .context_for_addon(&request, false, addon_id, addon_capture);
        self.match_count_filtered_with_context(request, context, filter)
    }

    fn match_count_filtered_with_context(
        &self,
        request: SearchRequest,
        context: SearchLogContext,
        filter: impl Fn(&Path) -> bool,
    ) -> Result<usize, SearchError> {
        let result = self
            .matching_paths(&request, &context, &filter)
            .map(|(_, paths)| paths.len());
        if let Err(error) = &result {
            self.logger.record_error(&context, &error.to_string());
        }
        result
    }

    fn matching_paths(
        &self,
        request: &SearchRequest,
        context: &SearchLogContext,
        filter: &dyn Fn(&Path) -> bool,
    ) -> Result<(String, Vec<PathBuf>), SearchError> {
        let query = request.query.trim();
        let expression = parse_query(query)?;
        if !request.fields.any() {
            return Err(SearchError::NoSearchFields);
        }
        if request.page == 0 || request.page_size != PAGE_SIZE {
            return Err(SearchError::InvalidPage);
        }

        let mut seen = HashSet::new();
        let needs_whole_path = request.fields.path || query_needs_whole_path(&expression);
        let mut paths = Vec::new();
        for path in self.candidates(query, needs_whole_path, context)? {
            if !is_supported_media(&path, request.media_type) {
                self.logger
                    .record_filtered(context, &path, "unsupported media type");
                continue;
            }
            if path.is_file() && !is_non_empty_file(&path) {
                self.logger.record_filtered(context, &path, "empty file");
                continue;
            }
            if !matches_query(&path, &expression, request.fields) {
                self.logger
                    .record_filtered(context, &path, "query did not match");
                continue;
            }
            if !filter(&path) {
                self.logger
                    .record_filtered(context, &path, "add-on filter did not match");
                continue;
            }
            if !seen.insert(path.clone()) {
                self.logger
                    .record_filtered(context, &path, "duplicate path");
                continue;
            }
            paths.push(path);
        }
        Ok((query.to_owned(), paths))
    }

    /// The seed the order of this page is drawn from. Page one is a new
    /// question and always gets a new one, so asking the same thing twice shows
    /// a different part of what matched rather than the same screenful again.
    /// The pages after it continue whatever order page one started.
    fn shuffle_seed(
        &self,
        query: &str,
        fields: SearchFields,
        media_type: MediaType,
        page: usize,
    ) -> u64 {
        let mut current = self.shuffle.lock().unwrap();
        if page > 1 {
            if let Some(shuffle) = current.as_ref() {
                if shuffle.query == query
                    && shuffle.fields == fields
                    && shuffle.media_type == media_type
                {
                    return shuffle.seed;
                }
            }
        }
        let seed = fresh_seed();
        *current = Some(Shuffle {
            query: query.to_owned(),
            fields,
            media_type,
            seed,
        });
        seed
    }

    /// The provider's candidates, corrected for renames Toka has recorded: a
    /// stale name is replaced by the name its file carries now, and every
    /// renamed file the index has already dropped is added back. The caller
    /// still filters these by the search terms, so a rename only shows up when
    /// the query matches the name the file actually has.
    fn candidates(
        &self,
        query: &str,
        whole_path: bool,
        context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError> {
        let mut paths = self.provider.candidates(query, whole_path, context)?;
        let indexed_paths: HashSet<PathBuf> = paths.iter().cloned().collect();
        let _update = self.rename_overlay_write.lock().unwrap();
        if let Some(path) = self.rename_overlay_path.as_deref() {
            match lock_rename_overlay(path) {
                Ok(file_lock) => {
                    self.reload_rename_overlay_locked(path);
                    drop(file_lock);
                }
                Err(error) => {
                    eprintln!("Toka could not lock pending search renames: {error}");
                }
            }
        }
        let should_prune = {
            let renamed = self.renamed_paths.lock().unwrap();
            let folders = self.renamed_folders.lock().unwrap();
            if renamed.is_empty() && folders.is_empty() {
                return Ok(paths);
            }
            for path in &mut paths {
                if let Some(current) = renamed.get(path) {
                    *path = current.clone();
                } else if let Some(current) = through_folders(folders.iter(), path) {
                    *path = current;
                }
            }
            let should_prune = renamed
                .values()
                .any(|current| !current.is_file() || indexed_paths.contains(current));
            // A renamed file that has since been deleted is not a search
            // result, and this is the only place a path Toka renamed itself
            // can be checked cheaply — provider candidates are deliberately
            // left untouched.
            paths.extend(
                renamed
                    .values()
                    .filter(|current| current.is_file() && !indexed_paths.contains(*current))
                    .cloned(),
            );
            // An index that drops names whose file is gone drops a renamed
            // folder's whole contents, so they are listed from the folder.
            for folder in folders.values() {
                files_under(folder, &mut paths);
            }
            should_prune
        };
        if should_prune {
            self.update_rename_overlay_locked(|renamed, _| {
                let before = renamed.len();
                renamed.retain(|_, current| current.is_file() && !indexed_paths.contains(current));
                renamed.len() != before
            });
        }
        Ok(paths)
    }

    fn reload_rename_overlay_locked(&self, path: &Path) -> bool {
        let overlay = match load_rename_overlay(path) {
            Ok(overlay) => overlay,
            Err(error) => {
                eprintln!("Toka could not refresh pending search renames: {error}");
                return false;
            }
        };
        let (renamed_paths, renamed_folders) = overlay.into_maps();
        *self.renamed_paths.lock().unwrap() = renamed_paths;
        *self.renamed_folders.lock().unwrap() = renamed_folders;
        true
    }

    fn update_rename_overlay(
        &self,
        update: impl Fn(&mut HashMap<PathBuf, PathBuf>, &mut HashMap<PathBuf, PathBuf>) -> bool,
    ) {
        let _write = self.rename_overlay_write.lock().unwrap();
        self.update_rename_overlay_locked(update);
    }

    fn update_rename_overlay_locked(
        &self,
        update: impl Fn(&mut HashMap<PathBuf, PathBuf>, &mut HashMap<PathBuf, PathBuf>) -> bool,
    ) {
        let Some(path) = self.rename_overlay_path.as_deref() else {
            let mut renamed = self.renamed_paths.lock().unwrap();
            let mut folders = self.renamed_folders.lock().unwrap();
            update(&mut renamed, &mut folders);
            return;
        };
        let file_lock = match lock_rename_overlay(path) {
            Ok(lock) => Some(lock),
            Err(error) => {
                eprintln!("Toka could not lock pending search renames: {error}");
                None
            }
        };
        let can_write = file_lock
            .as_ref()
            .is_some_and(|_| self.reload_rename_overlay_locked(path));
        let changed = {
            let mut renamed = self.renamed_paths.lock().unwrap();
            let mut folders = self.renamed_folders.lock().unwrap();
            update(&mut renamed, &mut folders)
        };
        if changed && can_write {
            self.persist_rename_overlay_locked(path);
        }
    }

    fn persist_rename_overlay_locked(&self, path: &Path) {
        let overlay = {
            let renamed = self.renamed_paths.lock().unwrap();
            let folders = self.renamed_folders.lock().unwrap();
            PersistedRenameOverlay::from_maps(&renamed, &folders)
        };
        if let Err(error) = write_rename_overlay(path, &overlay) {
            eprintln!("Toka could not save pending search renames: {error}");
        }
    }

    /// A page of results for videos Toka was handed rather than found, so a
    /// playlist file plays through everything a search's results do — the
    /// player, its drawer, tagging, thumbnails — without a second path through
    /// any of it. `paths` keeps the order it was given, because that is the
    /// order a playlist asked for.
    ///
    /// The page is whole: it holds every video there is, so nothing about it
    /// invites the frontend to ask for a second page that no search backs.
    pub fn page_of_videos(&self, query: String, paths: Vec<PathBuf>) -> SearchPage {
        let mut known_paths = self.result_paths.lock().unwrap();
        let results: Vec<VideoResult> = paths
            .into_iter()
            .map(|path| {
                let result = video_result(&path);
                known_paths.insert(result.id.clone(), path);
                result
            })
            .collect();
        SearchPage {
            query,
            page: 1,
            page_size: PAGE_SIZE,
            total_results: results.len(),
            total_pages: 1,
            index_revision: self.provider.revision(),
            results,
        }
    }

    pub fn video_path(&self, result_id: &str) -> Result<PathBuf, SearchError> {
        let path = self
            .result_paths
            .lock()
            .unwrap()
            .get(result_id)
            .cloned()
            .ok_or(SearchError::VideoUnavailable)?;
        if path.is_file() && is_non_empty_file(&path) && is_supported_media(&path, MediaType::Both)
        {
            Ok(path)
        } else {
            Err(SearchError::VideoUnavailable)
        }
    }

    pub fn update_video_path(&self, result_id: &str, path: PathBuf) -> Result<(), SearchError> {
        let mut known_paths = self.result_paths.lock().unwrap();
        let known_path = known_paths
            .get_mut(result_id)
            .ok_or(SearchError::VideoUnavailable)?;
        let previous = std::mem::replace(known_path, path.clone());
        if previous != path {
            self.record_rename(previous, path);
        }
        Ok(())
    }

    /// Records a rename under the name the index still knows the file by.
    /// Renaming the same file again replaces that one entry rather than
    /// chaining, and renaming it back to its indexed name drops the entry, so
    /// the index's own answer is enough again.
    fn record_rename(&self, previous: PathBuf, current: PathBuf) {
        self.update_rename_overlay(|renamed, folders| {
            let indexed = renamed
                .iter()
                .find(|(_, path)| **path == previous)
                .map(|(indexed, _)| indexed.clone())
                .or_else(|| through_folders(folders.iter().map(|(i, c)| (c, i)), &previous))
                .unwrap_or_else(|| previous.clone());
            if indexed == current {
                renamed.remove(&indexed).is_some()
            } else {
                renamed.insert(indexed, current.clone()).as_ref() != Some(&current)
            }
        });
    }

    /// Follows a folder from `previous` to `current`: every result the engine
    /// knows inside it now lives under the new name, and searches see it there
    /// while the index still has the old one. Returns the ids that moved.
    pub fn rename_folder(&self, previous: &Path, current: PathBuf) -> Vec<String> {
        let mut known_paths = self.result_paths.lock().unwrap();
        let mut moved = Vec::new();
        for (id, path) in known_paths.iter_mut() {
            if let Some(next) = moved_under(path, previous, &current) {
                *path = next;
                moved.push(id.clone());
            }
        }
        drop(known_paths);
        if previous == current {
            return moved;
        }
        self.update_rename_overlay(|renamed, folders| {
            for path in renamed.values_mut() {
                if let Some(next) = moved_under(path, previous, &current) {
                    *path = next;
                }
            }
            renamed.retain(|indexed, path| indexed != path);
            let indexed = through_folders(folders.iter().map(|(i, c)| (c, i)), previous)
                .unwrap_or_else(|| previous.to_path_buf());
            for path in folders.values_mut() {
                if let Some(next) = moved_under(path, previous, &current) {
                    *path = next;
                }
            }
            folders.insert(indexed, current.clone());
            folders.retain(|indexed, path| indexed != path);
            true
        });
        moved
    }

    pub fn thumbnail_path(&self, result_id: &str) -> Result<PathBuf, SearchError> {
        let path = self.video_path(result_id)?;
        if is_supported_image(&path) {
            return Ok(path);
        }
        thumbnails::generate(&path).ok_or(SearchError::VideoUnavailable)
    }

    /// The frames a preview runs through for the video behind `result_id`.
    pub fn preview_paths(&self, result_id: &str) -> Result<Vec<PathBuf>, SearchError> {
        let path = self.video_path(result_id)?;
        if is_supported_image(&path) {
            return Ok(vec![path]);
        }
        thumbnails::preview(&path).ok_or(SearchError::VideoUnavailable)
    }
}

/// A seed nothing can predict, and never the same one twice: the clock alone
/// repeats itself when two searches land inside the same tick, which the
/// counter separates.
fn fresh_seed() -> u64 {
    static SEARCHES: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos() as u64)
        .unwrap_or_default();
    let searches = SEARCHES.fetch_add(1, Ordering::Relaxed);
    let mut state = now ^ searches.wrapping_mul(GOLDEN_GAMMA);
    next_random(&mut state)
}

/// Toka carries no random number generator, and does not need one it could not
/// reproduce: SplitMix64 is a handful of arithmetic, and a seed is enough to
/// draw the same order again for the pages that follow.
const GOLDEN_GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

fn next_random(state: &mut u64) -> u64 {
    *state = state.wrapping_add(GOLDEN_GAMMA);
    let mut drawn = *state;
    drawn = (drawn ^ (drawn >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    drawn = (drawn ^ (drawn >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    drawn ^ (drawn >> 31)
}

/// Fisher-Yates, so every order is as likely as every other one.
fn shuffle(paths: &mut [PathBuf], seed: u64) {
    let mut state = seed;
    for index in (1..paths.len()).rev() {
        let swap = (next_random(&mut state) % (index as u64 + 1)) as usize;
        paths.swap(index, swap);
    }
}

/// A result for `path` under a fresh id, which is what the frontend names the
/// video by from then on: an id the engine minted, never a filesystem path.
fn video_result(path: &Path) -> VideoResult {
    VideoResult {
        id: Uuid::new_v4().to_string(),
        file_name: path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        extension: path
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase(),
        thumbnail_path: if is_supported_video(path) {
            thumbnails::cached(path).map(|thumbnail| thumbnail.to_string_lossy().into_owned())
        } else {
            None
        },
        tags: tags::get(path),
        folder_tags: path
            .parent()
            .and_then(Path::file_name)
            .map(|name| tags::Tags::parse_dir_name(&name.to_string_lossy()).into_values())
            .unwrap_or_default(),
    }
}

/// `path` moved from under `from` to the same place under `to`, if it was
/// under `from` at all. Compared by component, so `/a/b` is not under `/a/bc`.
fn moved_under(path: &Path, from: &Path, to: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix(from).ok()?;
    Some(if rest.as_os_str().is_empty() {
        to.to_path_buf()
    } else {
        to.join(rest)
    })
}

/// The deepest renamed folder `path` sits under, looked up from one side of a
/// rename (the indexed name or the current one) and moved to the other side.
fn through_folders<'a>(
    folders: impl Iterator<Item = (&'a PathBuf, &'a PathBuf)>,
    path: &Path,
) -> Option<PathBuf> {
    folders
        .filter(|(from, _)| path.starts_with(from))
        .max_by_key(|(from, _)| from.components().count())
        .and_then(|(from, to)| moved_under(path, from, to))
}

/// Every file below `folder`, without following links to other folders.
fn files_under(folder: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => files_under(&entry.path(), files),
            Ok(kind) if kind.is_file() => files.push(entry.path()),
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryField {
    Tags,
    FileName,
    Path,
    Folder,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    Equal,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateField {
    Created,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CalendarDate {
    year: i32,
    month: u32,
    day: u32,
}

impl CalendarDate {
    fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        if year < 1
            || !(1..=12).contains(&month)
            || !(1..=days_in_month(year, month)).contains(&day)
        {
            return None;
        }
        Some(Self { year, month, day })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueryMediaType {
    Video,
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryTerm {
    Text {
        field: QueryField,
        value: String,
    },
    MediaType(QueryMediaType),
    Extension(String),
    Size {
        comparison: Comparison,
        bytes: u64,
    },
    Date {
        field: DateField,
        comparison: Comparison,
        date: CalendarDate,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryExpr {
    Term(QueryTerm),
    Not(Box<QueryExpr>),
    And(Box<QueryExpr>, Box<QueryExpr>),
    Or(Box<QueryExpr>, Box<QueryExpr>),
}

pub(crate) fn tokenize_query(query: &str) -> Result<Vec<String>, SearchError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut characters = query.chars().peekable();
    let mut quoted = false;

    while let Some(character) = characters.next() {
        match character {
            '"' => quoted = !quoted,
            '\\' if quoted => match characters.peek().copied() {
                Some('"' | '\\') => current.push(characters.next().unwrap()),
                Some(_) => current.push(character),
                None => return Err(SearchError::InvalidQuery),
            },
            '(' | ')' if !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(character.to_string());
            }
            character if character.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(character),
        }
    }

    if quoted {
        return Err(SearchError::InvalidQuery);
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

fn parse_query(query: &str) -> Result<QueryExpr, SearchError> {
    let today = calendar_date(SystemTime::now()).ok_or(SearchError::InvalidQuery)?;
    parse_query_at(query, today)
}

fn parse_query_at(query: &str, today: CalendarDate) -> Result<QueryExpr, SearchError> {
    let tokens = tokenize_query(query)?;
    if tokens.is_empty() {
        return Err(SearchError::InvalidQuery);
    }
    let mut parser = QueryParser {
        tokens,
        position: 0,
        today,
    };
    let expression = parser.parse_or()?;
    if parser.position != parser.tokens.len() {
        return Err(SearchError::InvalidQuery);
    }
    Ok(expression)
}

struct QueryParser {
    tokens: Vec<String>,
    position: usize,
    today: CalendarDate,
}

impl QueryParser {
    fn peek(&self) -> Option<&str> {
        self.tokens.get(self.position).map(String::as_str)
    }

    fn consume(&mut self) -> Option<String> {
        let token = self.tokens.get(self.position).cloned()?;
        self.position += 1;
        Some(token)
    }

    fn parse_or(&mut self) -> Result<QueryExpr, SearchError> {
        let mut expression = self.parse_and()?;
        while self
            .peek()
            .is_some_and(|token| token.eq_ignore_ascii_case("OR"))
        {
            self.consume();
            expression = QueryExpr::Or(Box::new(expression), Box::new(self.parse_and()?));
        }
        Ok(expression)
    }

    fn parse_and(&mut self) -> Result<QueryExpr, SearchError> {
        let mut expression = self.parse_not()?;
        loop {
            match self.peek() {
                None | Some(")") => break,
                Some(token) if token.eq_ignore_ascii_case("OR") => break,
                Some(token) if token.eq_ignore_ascii_case("AND") => {
                    self.consume();
                }
                Some(_) => {}
            }
            expression = QueryExpr::And(Box::new(expression), Box::new(self.parse_not()?));
        }
        Ok(expression)
    }

    fn parse_not(&mut self) -> Result<QueryExpr, SearchError> {
        if self
            .peek()
            .is_some_and(|token| token.eq_ignore_ascii_case("NOT"))
        {
            self.consume();
            return Ok(QueryExpr::Not(Box::new(self.parse_not()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<QueryExpr, SearchError> {
        match self.peek() {
            Some("(") => {
                self.consume();
                let expression = self.parse_or()?;
                if self.consume().as_deref() != Some(")") {
                    return Err(SearchError::InvalidQuery);
                }
                Ok(expression)
            }
            Some(")") | None => Err(SearchError::InvalidQuery),
            Some(token)
                if token.eq_ignore_ascii_case("AND")
                    || token.eq_ignore_ascii_case("OR")
                    || token.eq_ignore_ascii_case("NOT") =>
            {
                Err(SearchError::InvalidQuery)
            }
            Some(_) => parse_query_term(&self.consume().unwrap(), self.today),
        }
    }
}

fn parse_query_term(token: &str, today: CalendarDate) -> Result<QueryExpr, SearchError> {
    if let Some((raw_field, value)) = token.split_once(':') {
        let field = raw_field.to_ascii_lowercase();
        let term = match field.as_str() {
            "size" => parse_size_term(value)?,
            "created" => parse_date_term(DateField::Created, value, today)?,
            "modified" => parse_date_term(DateField::Modified, value, today)?,
            "type" => QueryTerm::MediaType(parse_media_type(value)?),
            "extension" | "ext" => QueryTerm::Extension(parse_extension(value)?),
            "tags" | "tag" | "filename" | "file" | "name" | "path" | "folder" => {
                if value.is_empty() {
                    return Err(SearchError::InvalidQuery);
                }
                let field = match field.as_str() {
                    "tags" | "tag" => QueryField::Tags,
                    "filename" | "file" | "name" => QueryField::FileName,
                    "path" => QueryField::Path,
                    "folder" => QueryField::Folder,
                    _ => unreachable!(),
                };
                QueryTerm::Text {
                    field,
                    value: value.to_lowercase(),
                }
            }
            _ => QueryTerm::Text {
                field: QueryField::Any,
                value: token.to_lowercase(),
            },
        };
        return Ok(QueryExpr::Term(term));
    }

    Ok(QueryExpr::Term(QueryTerm::Text {
        field: QueryField::Any,
        value: token.to_lowercase(),
    }))
}

fn parse_media_type(value: &str) -> Result<QueryMediaType, SearchError> {
    match value.to_ascii_lowercase().as_str() {
        "video" | "videos" => Ok(QueryMediaType::Video),
        "image" | "images" => Ok(QueryMediaType::Image),
        _ => Err(SearchError::InvalidQuery),
    }
}

fn parse_extension(value: &str) -> Result<String, SearchError> {
    let value = value.strip_prefix('.').unwrap_or(value);
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return Err(SearchError::InvalidQuery);
    }
    Ok(value.to_lowercase())
}

fn parse_size_term(value: &str) -> Result<QueryTerm, SearchError> {
    let (comparison, number) = split_comparison(value);
    let number = number.to_ascii_lowercase();
    let units = [
        ("tib", 1_099_511_627_776u64),
        ("tb", 1_000_000_000_000u64),
        ("gib", 1_073_741_824u64),
        ("gb", 1_000_000_000u64),
        ("mib", 1_048_576u64),
        ("mb", 1_000_000u64),
        ("kib", 1_024u64),
        ("kb", 1_000u64),
        ("b", 1u64),
        ("", 1u64),
    ];
    let (number, multiplier) = units
        .iter()
        .find_map(|(suffix, multiplier)| {
            number
                .strip_suffix(suffix)
                .map(|number| (number, *multiplier))
        })
        .ok_or(SearchError::InvalidQuery)?;
    let number = number
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite() && *number >= 0.0)
        .ok_or(SearchError::InvalidQuery)?;
    let bytes = number * multiplier as f64;
    if !bytes.is_finite() || bytes > u64::MAX as f64 {
        return Err(SearchError::InvalidQuery);
    }
    Ok(QueryTerm::Size {
        comparison,
        bytes: bytes.round() as u64,
    })
}

fn parse_date_term(
    field: DateField,
    value: &str,
    today: CalendarDate,
) -> Result<QueryTerm, SearchError> {
    let (comparison, value) = split_comparison(value);
    let date = parse_calendar_date(value).or_else(|_| parse_relative_date(value, today))?;
    Ok(QueryTerm::Date {
        field,
        comparison,
        date,
    })
}

fn parse_relative_date(value: &str, today: CalendarDate) -> Result<CalendarDate, SearchError> {
    let lower = value.to_ascii_lowercase();
    let parts = lower.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 3 || parts[2] != "ago" {
        return Err(SearchError::InvalidQuery);
    }
    let amount = parts[0]
        .parse::<u64>()
        .ok()
        .filter(|amount| *amount > 0)
        .ok_or(SearchError::InvalidQuery)?;
    let multiplier = match parts[1] {
        "day" | "days" => 1,
        "week" | "weeks" => 7,
        _ => return Err(SearchError::InvalidQuery),
    };
    let days = amount
        .checked_mul(multiplier)
        .ok_or(SearchError::InvalidQuery)?;
    subtract_calendar_days(today, days)
}

fn subtract_calendar_days(
    mut date: CalendarDate,
    mut days: u64,
) -> Result<CalendarDate, SearchError> {
    while days >= u64::from(date.day) {
        days -= u64::from(date.day);
        if date.month == 1 {
            date.year = date.year.checked_sub(1).ok_or(SearchError::InvalidQuery)?;
            if date.year < 1 {
                return Err(SearchError::InvalidQuery);
            }
            date.month = 12;
        } else {
            date.month -= 1;
        }
        date.day = days_in_month(date.year, date.month);
    }
    date.day -= days as u32;
    CalendarDate::new(date.year, date.month, date.day).ok_or(SearchError::InvalidQuery)
}

fn parse_calendar_date(value: &str) -> Result<CalendarDate, SearchError> {
    let parts = value.split('-').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(SearchError::InvalidQuery);
    }

    let (year, month, day) = if parts[0].len() == 4 {
        (
            parse_year(parts[0], false)?,
            parse_month_or_day(parts[1])?,
            parse_month_or_day(parts[2])?,
        )
    } else {
        (
            parse_year(parts[2], parts[2].len() == 2)?,
            parse_month_or_day(parts[0])?,
            parse_month_or_day(parts[1])?,
        )
    };

    CalendarDate::new(year, month, day).ok_or(SearchError::InvalidQuery)
}

fn parse_year(value: &str, allow_two_digits: bool) -> Result<i32, SearchError> {
    let expected_length = if allow_two_digits { 2 } else { 4 };
    if value.len() != expected_length || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SearchError::InvalidQuery);
    }
    let year = value
        .parse::<i32>()
        .map_err(|_| SearchError::InvalidQuery)?;
    if allow_two_digits {
        Ok(if year <= 68 { 2000 + year } else { 1900 + year })
    } else {
        Ok(year)
    }
}

fn parse_month_or_day(value: &str) -> Result<u32, SearchError> {
    if !(1..=2).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SearchError::InvalidQuery);
    }
    value.parse::<u32>().map_err(|_| SearchError::InvalidQuery)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(unix)]
fn calendar_date(time: SystemTime) -> Option<CalendarDate> {
    let seconds = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().try_into().ok()?,
        Err(error) => {
            let duration = error.duration();
            let seconds = duration
                .as_secs()
                .checked_add(u64::from(duration.subsec_nanos() != 0))?;
            let seconds: libc::time_t = seconds.try_into().ok()?;
            seconds.checked_neg()?
        }
    };
    let mut local = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&seconds, &mut local) }.is_null() {
        return None;
    }
    CalendarDate::new(
        local.tm_year.checked_add(1900)?,
        u32::try_from(local.tm_mon.checked_add(1)?).ok()?,
        u32::try_from(local.tm_mday).ok()?,
    )
}

#[cfg(not(unix))]
fn calendar_date(_time: SystemTime) -> Option<CalendarDate> {
    None
}

fn split_comparison(value: &str) -> (Comparison, &str) {
    for (prefix, comparison) in [
        ("<=", Comparison::LessOrEqual),
        (">=", Comparison::GreaterOrEqual),
        ("<", Comparison::Less),
        (">", Comparison::Greater),
        ("=", Comparison::Equal),
    ] {
        if let Some(value) = value.strip_prefix(prefix) {
            return (comparison, value);
        }
    }
    (Comparison::Equal, value)
}

fn query_needs_whole_path(expression: &QueryExpr) -> bool {
    match expression {
        QueryExpr::Term(QueryTerm::Text { field, .. }) => {
            matches!(field, QueryField::Path | QueryField::Folder)
        }
        QueryExpr::Term(
            QueryTerm::MediaType(_)
            | QueryTerm::Extension(_)
            | QueryTerm::Size { .. }
            | QueryTerm::Date { .. },
        ) => false,
        QueryExpr::Not(expression) => query_needs_whole_path(expression),
        QueryExpr::And(left, right) | QueryExpr::Or(left, right) => {
            query_needs_whole_path(left) || query_needs_whole_path(right)
        }
    }
}

fn matches_query(path: &Path, expression: &QueryExpr, fields: SearchFields) -> bool {
    match expression {
        QueryExpr::Term(QueryTerm::Text { field, value }) => match field {
            QueryField::Any => haystacks(path, fields)
                .iter()
                .any(|hay| hay.contains(value)),
            QueryField::Tags => tags_haystack(path).contains(value),
            QueryField::FileName => file_name_haystack(path).contains(value),
            QueryField::Path | QueryField::Folder => path_haystack(path).contains(value),
        },
        QueryExpr::Term(QueryTerm::MediaType(media_type)) => match media_type {
            QueryMediaType::Video => is_supported_video(path),
            QueryMediaType::Image => is_supported_image(path),
        },
        QueryExpr::Term(QueryTerm::Extension(extension)) => path
            .extension()
            .is_some_and(|actual| actual.to_string_lossy().eq_ignore_ascii_case(extension)),
        QueryExpr::Term(QueryTerm::Size { comparison, bytes }) => fs::metadata(path)
            .map(|metadata| compare(metadata.len(), *comparison, *bytes))
            .unwrap_or(false),
        QueryExpr::Term(QueryTerm::Date {
            field,
            comparison,
            date,
        }) => fs::metadata(path)
            .ok()
            .and_then(|metadata| {
                let time = match field {
                    DateField::Created => metadata.created().ok()?,
                    DateField::Modified => metadata.modified().ok()?,
                };
                calendar_date(time)
            })
            .is_some_and(|actual| compare(actual, *comparison, *date)),
        QueryExpr::Not(expression) => !matches_query(path, expression, fields),
        QueryExpr::And(left, right) => {
            matches_query(path, left, fields) && matches_query(path, right, fields)
        }
        QueryExpr::Or(left, right) => {
            matches_query(path, left, fields) || matches_query(path, right, fields)
        }
    }
}

fn compare<T: PartialOrd>(actual: T, comparison: Comparison, expected: T) -> bool {
    match comparison {
        Comparison::Equal => actual == expected,
        Comparison::Less => actual < expected,
        Comparison::LessOrEqual => actual <= expected,
        Comparison::Greater => actual > expected,
        Comparison::GreaterOrEqual => actual >= expected,
    }
}

/// The lowercased text of each part of `path` that is being searched. The tags
/// are read from the name rather than the filesystem, so this stays free of the
/// per-candidate `stat` a lookup would cost.
fn haystacks(path: &Path, fields: SearchFields) -> Vec<String> {
    let mut haystacks = Vec::with_capacity(3);
    if fields.tags {
        haystacks.push(tags_haystack(path));
    }
    if fields.file_name {
        haystacks.push(file_name_haystack(path));
    }
    if fields.path {
        haystacks.push(path_haystack(path));
    }
    haystacks
}

fn tags_haystack(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    tags::Tags::parse_file_name(&name)
        .into_values()
        .join(" ")
        .to_lowercase()
}

fn file_name_haystack(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    tags::Tags::default().apply_to_file(&name).to_lowercase()
}

fn path_haystack(path: &Path) -> String {
    path.parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .to_lowercase()
}

pub fn is_supported_video(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_lowercase)
            .as_deref(),
        Some("mp4" | "mov" | "mkv" | "avi" | "webm" | "m4v" | "mpeg" | "mpg" | "mpe")
    )
}

pub fn is_supported_image(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_lowercase)
            .as_deref(),
        Some(
            "jpg"
                | "jpeg"
                | "png"
                | "webp"
                | "gif"
                | "bmp"
                | "tiff"
                | "tif"
                | "heic"
                | "heif"
                | "avif"
        )
    )
}

pub fn is_supported_media(path: &Path, media_type: MediaType) -> bool {
    match media_type {
        MediaType::Videos => is_supported_video(path),
        MediaType::Images => is_supported_image(path),
        MediaType::Both => is_supported_video(path) || is_supported_image(path),
    }
}

pub(crate) fn is_non_empty_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::Mutex};
    use tempfile::tempdir;

    struct FakeProvider {
        paths: Mutex<Vec<PathBuf>>,
        whole_path_asks: Mutex<Vec<bool>>,
    }

    impl FakeProvider {
        fn new(paths: Vec<PathBuf>) -> Self {
            Self {
                paths: Mutex::new(paths),
                whole_path_asks: Mutex::new(Vec::new()),
            }
        }
    }

    impl SearchProvider for FakeProvider {
        fn candidates(
            &self,
            _query: &str,
            whole_path: bool,
            _context: &SearchLogContext,
        ) -> Result<Vec<PathBuf>, SearchError> {
            self.whole_path_asks.lock().unwrap().push(whole_path);
            Ok(self.paths.lock().unwrap().clone())
        }
    }

    fn request(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.into(),
            page: 1,
            page_size: PAGE_SIZE,
            fields: SearchFields::default(),
            media_type: MediaType::default(),
        }
    }

    struct ErrorProvider;

    impl SearchProvider for ErrorProvider {
        fn candidates(
            &self,
            _query: &str,
            _whole_path: bool,
            _context: &SearchLogContext,
        ) -> Result<Vec<PathBuf>, SearchError> {
            Err(SearchError::Provider("backend unavailable".into()))
        }
    }

    #[test]
    fn records_search_provider_errors_with_the_original_request() {
        let directory = tempdir().unwrap();
        let log_path = directory.path().join("search.log");
        let logger = Arc::new(SearchLogger::new(log_path.clone()));
        let engine = SearchEngine::new_with_logger(Arc::new(ErrorProvider), logger);
        let mut search = request("  summer vacation  ");
        search.fields = SearchFields {
            tags: false,
            file_name: true,
            path: true,
        };
        search.media_type = MediaType::Both;

        assert_eq!(
            engine.search(search).unwrap_err().to_string(),
            "backend unavailable"
        );
        let line = fs::read_to_string(log_path).unwrap();
        let entry: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(entry["kind"], "error");
        assert_eq!(entry["query"], "  summer vacation  ");
        assert_eq!(entry["fields"]["tags"], false);
        assert_eq!(entry["fields"]["fileName"], true);
        assert_eq!(entry["fields"]["path"], true);
        assert_eq!(entry["mediaType"], "both");
        assert_eq!(entry["error"], "backend unavailable");
    }

    #[test]
    fn records_addon_identity_with_filtered_search_errors() {
        let directory = tempdir().unwrap();
        let log_path = directory.path().join("search.log");
        let logger = Arc::new(SearchLogger::new(log_path.clone()));
        let engine = SearchEngine::new_with_logger(Arc::new(ErrorProvider), logger);

        engine
            .search_filtered_for_addon(request("@sam"), "tiktok", "handle", |_| true)
            .unwrap_err();

        let line = fs::read_to_string(log_path).unwrap();
        let entry: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(entry["addonId"], "tiktok");
        assert_eq!(entry["addonCapture"], "handle");
    }

    #[test]
    fn search_returns_only_supported_files_containing_every_term() {
        let directory = tempdir().unwrap();
        let matching = directory.path().join("Summer Family Vacation.MP4");
        let missing_term = directory.path().join("summer-party.mov");
        let wrong_type = directory.path().join("summer-family-vacation.txt");
        for path in [&matching, &missing_term, &wrong_type] {
            fs::write(path, b"test").unwrap();
        }

        let provider = Arc::new(FakeProvider::new(vec![
            wrong_type,
            missing_term,
            matching.clone(),
        ]));
        let page = SearchEngine::new(provider)
            .search(request("family SUMMER"))
            .unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "Summer Family Vacation.MP4");
        assert_eq!(page.results[0].extension, "mp4");
        assert!(!page.results[0].id.is_empty());
    }

    #[test]
    fn an_addon_filter_runs_before_shuffle_and_paging() {
        let directory = tempdir().unwrap();
        let mut paths = Vec::new();
        for number in 0..30 {
            for handle in ["@sam", "@sammy"] {
                let path = directory
                    .path()
                    .join(format!("2026-10-10_{handle}_{number:020}.mp4"));
                fs::write(&path, b"test").unwrap();
                paths.push(path);
            }
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));
        let mut search = request("@sam");
        search.fields = SearchFields {
            tags: false,
            file_name: true,
            path: false,
        };

        let page = engine
            .search_filtered(search, |path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains("_@sam_"))
            })
            .unwrap();

        assert_eq!(page.total_results, 30);
        assert_eq!(page.total_pages, 2);
        assert_eq!(page.results.len(), PAGE_SIZE);
        assert!(page
            .results
            .iter()
            .all(|result| result.file_name.contains("_@sam_")));
    }

    #[test]
    fn search_skips_empty_media_files() {
        let directory = tempdir().unwrap();
        let empty = directory.path().join("empty.mp4");
        let matching = directory.path().join("matching.mp4");
        fs::write(&empty, b"").unwrap();
        fs::write(&matching, b"video").unwrap();

        let page = SearchEngine::new(Arc::new(FakeProvider::new(vec![empty, matching])))
            .search(request("mp4"))
            .unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "matching.mp4");
    }

    #[test]
    fn search_lists_provider_video_paths_without_filesystem_access() {
        let protected = PathBuf::from("/protected/Downloads/Untitled.mov");
        let provider = Arc::new(FakeProvider::new(vec![protected.clone()]));

        let page = SearchEngine::new(provider)
            .search(request("untitled"))
            .unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "Untitled.mov");
        assert_eq!(page.results[0].extension, "mov");
    }

    #[test]
    fn match_count_checks_current_results_without_changing_search_order() {
        let directory = tempdir().unwrap();
        let paths = clips(directory.path(), 25);
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));

        assert_eq!(engine.match_count(request("clip")).unwrap(), 25);
        let first = engine.search(request("clip")).unwrap();
        assert_eq!(first.total_results, 25);
    }

    #[test]
    fn search_reports_a_thumbnail_the_background_indexer_already_generated() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("clip.mp4");
        fs::write(&video, b"test").unwrap();
        let cached = thumbnails::cache_path(&video).unwrap();
        fs::write(&cached, b"thumbnail").unwrap();

        let page = SearchEngine::new(Arc::new(FakeProvider::new(vec![video])))
            .search(request("clip"))
            .unwrap();

        assert_eq!(
            page.results[0].thumbnail_path,
            Some(cached.to_string_lossy().into_owned())
        );
        let _ = fs::remove_file(cached);
    }

    /// `count` clips named `clip-00.mp4` upwards, handed to the provider in
    /// reverse so nothing downstream can pass by keeping the order it was given.
    fn clips(directory: &Path, count: usize) -> Vec<PathBuf> {
        (0..count)
            .rev()
            .map(|number| {
                let path = directory.join(format!("clip-{number:02}.mp4"));
                fs::write(&path, b"test").unwrap();
                path
            })
            .collect()
    }

    fn names(page: &SearchPage) -> Vec<String> {
        page.results
            .iter()
            .map(|result| result.file_name.clone())
            .collect()
    }

    /// Every name `clips` wrote, in the order `sort` puts them in, so a set of
    /// results can be compared for what it holds rather than for its order.
    fn every_clip_name(count: usize) -> Vec<String> {
        (0..count)
            .map(|number| format!("clip-{number:02}.mp4"))
            .collect()
    }

    fn page_two(query: &str) -> SearchRequest {
        SearchRequest {
            page: 2,
            ..request(query)
        }
    }

    #[test]
    fn search_deduplicates_and_paginates_results() {
        let directory = tempdir().unwrap();
        let mut paths = clips(directory.path(), 25);
        paths.push(paths[0].clone());

        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));
        let first = engine.search(request("clip")).unwrap();
        let second = engine.search(page_two("clip")).unwrap();

        assert_eq!(first.total_results, 25);
        assert_eq!(first.total_pages, 2);
        assert_eq!(first.results.len(), 24);
        assert_eq!(second.results.len(), 1);

        let mut every_name = [names(&first), names(&second)].concat();
        every_name.sort();
        assert_eq!(every_name, every_clip_name(25));
    }

    #[test]
    fn search_keeps_the_page_size_and_rejects_invalid_page_requests() {
        let directory = tempdir().unwrap();
        let paths = clips(directory.path(), PAGE_SIZE + 1);
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));

        let first = engine.search(request("clip")).unwrap();
        assert_eq!(first.page, 1);
        assert_eq!(first.page_size, 24);
        assert_eq!(first.total_results, 25);
        assert_eq!(first.total_pages, 2);

        let mut zero_page = request("clip");
        zero_page.page = 0;
        assert!(matches!(
            engine.search(zero_page),
            Err(SearchError::InvalidPage)
        ));

        let mut wrong_size = request("clip");
        wrong_size.page_size = 12;
        assert!(matches!(
            engine.search(wrong_size),
            Err(SearchError::InvalidPage)
        ));
    }

    /// An alphabetical list buries everything past the first screenful, which
    /// is the wrong answer to "find me something to watch".
    #[test]
    fn search_shuffles_results_rather_than_listing_them_by_name() {
        let directory = tempdir().unwrap();
        let paths = clips(directory.path(), 30);

        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));
        let found = names(&engine.search(request("clip")).unwrap());

        let mut by_name = found.clone();
        by_name.sort();
        assert_eq!(found.len(), 24);
        assert_ne!(found, by_name);
    }

    /// The pages after the first continue the order the first one started. A
    /// fresh shuffle per page would repeat some videos and skip others.
    #[test]
    fn search_keeps_one_order_across_the_pages_of_a_search() {
        let directory = tempdir().unwrap();
        let paths = clips(directory.path(), 30);

        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));
        let first = names(&engine.search(request("clip")).unwrap());
        let second = names(&engine.search(page_two("clip")).unwrap());

        let mut every_name = [first, second].concat();
        every_name.sort();
        assert_eq!(every_name, every_clip_name(30));
    }

    /// Asking the same question again is a new search, and gets a new order —
    /// otherwise a viewer who did not like what came back has no way to see the
    /// rest of what matched.
    #[test]
    fn search_gives_the_same_query_a_new_order_each_time() {
        let directory = tempdir().unwrap();
        let paths = clips(directory.path(), 30);

        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));
        let first = names(&engine.search(request("clip")).unwrap());
        let second = names(&engine.search(request("clip")).unwrap());

        assert_ne!(first, second);
    }

    #[test]
    fn result_ids_are_opaque_and_do_not_reveal_paths() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("private-name.mp4");
        fs::write(&video, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![video.clone()])));

        let page = engine.search(request("private-name")).unwrap();
        let result = &page.results[0];

        assert!(!result.id.contains("private-name"));
        assert!(!result.id.contains(&video.to_string_lossy().into_owned()));
        assert_eq!(engine.video_path(&result.id).unwrap(), video);
    }

    #[test]
    fn search_covers_tags_filename_and_path_by_default() {
        let directory = tempdir().unwrap();
        let folder = directory.path().join("holiday-footage");
        fs::create_dir(&folder).unwrap();
        let video = folder.join("Beach day [home].mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![video]));
        let engine = SearchEngine::new(provider.clone());

        assert_eq!(
            engine
                .search(request("beach holiday-footage"))
                .unwrap()
                .total_results,
            1
        );
        assert_eq!(*provider.whole_path_asks.lock().unwrap(), [true]);
    }

    #[test]
    fn searching_tags_alone_ignores_the_rest_of_the_name() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("Beach day [home].mp4");
        fs::write(&video, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![video])));
        let tags_only = |query: &str| SearchRequest {
            fields: SearchFields {
                tags: true,
                file_name: false,
                path: false,
            },
            ..request(query)
        };

        assert_eq!(engine.search(tags_only("home")).unwrap().total_results, 1);
        assert_eq!(engine.search(tags_only("beach")).unwrap().total_results, 0);
    }

    #[test]
    fn searching_the_file_name_alone_ignores_its_tag_block() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("Beach day [home].mp4");
        fs::write(&video, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![video])));
        let name_only = |query: &str| SearchRequest {
            fields: SearchFields {
                tags: false,
                file_name: true,
                path: false,
            },
            ..request(query)
        };

        assert_eq!(engine.search(name_only("beach")).unwrap().total_results, 1);
        assert_eq!(engine.search(name_only("home")).unwrap().total_results, 0);
    }

    #[test]
    fn searching_the_path_matches_the_folder_and_widens_the_candidate_set() {
        let directory = tempdir().unwrap();
        let folder = directory.path().join("Holiday");
        fs::create_dir(&folder).unwrap();
        let video = folder.join("clip.mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![video]));
        let engine = SearchEngine::new(provider.clone());
        let with_path = |path: bool| SearchRequest {
            fields: SearchFields {
                tags: true,
                file_name: true,
                path,
            },
            ..request("holiday")
        };

        assert_eq!(engine.search(with_path(false)).unwrap().total_results, 0);
        assert_eq!(engine.search(with_path(true)).unwrap().total_results, 1);
        assert_eq!(*provider.whole_path_asks.lock().unwrap(), [false, true]);
    }

    #[test]
    fn path_search_covers_every_ancestor_folder() {
        let directory = tempdir().unwrap();
        let parent = directory.path().join("Family Archive");
        let grandchild = parent.join("2026").join("October");
        fs::create_dir_all(&grandchild).unwrap();
        let video = grandchild.join("clip.mp4");
        fs::write(&video, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![video])));
        let path_only = |query: &str| SearchRequest {
            fields: SearchFields {
                tags: false,
                file_name: false,
                path: true,
            },
            ..request(query)
        };

        assert_eq!(
            engine
                .search(path_only("family october"))
                .unwrap()
                .total_results,
            1
        );
        assert_eq!(
            engine.search(path_only("missing")).unwrap().total_results,
            0
        );
    }

    #[test]
    fn every_term_may_match_a_different_selected_field() {
        let directory = tempdir().unwrap();
        let folder = directory.path().join("Holiday");
        fs::create_dir(&folder).unwrap();
        let video = folder.join("clip [home].mp4");
        fs::write(&video, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![video])));
        let all_fields = |query: &str| SearchRequest {
            fields: SearchFields {
                tags: true,
                file_name: true,
                path: true,
            },
            ..request(query)
        };

        assert_eq!(
            engine
                .search(all_fields("holiday clip home"))
                .unwrap()
                .total_results,
            1
        );
        assert_eq!(
            engine
                .search(all_fields("holiday absent"))
                .unwrap()
                .total_results,
            0
        );
    }

    #[test]
    fn advanced_search_or_matches_either_branch() {
        let directory = tempdir().unwrap();
        let summer = directory.path().join("summer.mp4");
        let winter = directory.path().join("winter.mp4");
        let spring = directory.path().join("spring.mp4");
        for path in [&summer, &winter, &spring] {
            fs::write(path, b"test").unwrap();
        }

        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![
            summer.clone(),
            winter.clone(),
            spring,
        ])));
        let page = engine.search(request("summer OR winter")).unwrap();

        let mut found = names(&page);
        found.sort();
        assert_eq!(found, ["summer.mp4", "winter.mp4"]);
    }

    #[test]
    fn explicit_and_matches_implicit_and() {
        let directory = tempdir().unwrap();
        let matching = directory.path().join("summer holiday.mp4");
        let partial = directory.path().join("summer work.mp4");
        for path in [&matching, &partial] {
            fs::write(path, b"test").unwrap();
        }
        let paths = vec![matching, partial];
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(paths)));

        assert_eq!(
            names(&engine.search(request("summer AND holiday")).unwrap()),
            names(&engine.search(request("summer holiday")).unwrap())
        );
    }

    #[test]
    fn advanced_search_and_binds_tighter_than_or() {
        let directory = tempdir().unwrap();
        let summer = directory.path().join("summer.mp4");
        let winter_beach = directory.path().join("winter beach.mp4");
        let winter_party = directory.path().join("winter party.mp4");
        for path in [&summer, &winter_beach, &winter_party] {
            fs::write(path, b"test").unwrap();
        }

        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![
            summer,
            winter_beach,
            winter_party,
        ])));
        let page = engine.search(request("summer OR winter beach")).unwrap();

        let mut found = names(&page);
        found.sort();
        assert_eq!(found, ["summer.mp4", "winter beach.mp4"]);
    }

    #[test]
    fn advanced_search_supports_parentheses_and_field_qualifiers() {
        let directory = tempdir().unwrap();
        let holiday = directory.path().join("Holiday");
        let other = directory.path().join("Other");
        fs::create_dir(&holiday).unwrap();
        fs::create_dir(&other).unwrap();
        let holiday_home = holiday.join("clip [home].mp4");
        let holiday_work = holiday.join("clip [work].mp4");
        let other_home = other.join("different [home].mp4");
        let other_filename = other.join("home clip.mp4");
        for path in [&holiday_home, &holiday_work, &other_home, &other_filename] {
            fs::write(path, b"test").unwrap();
        }

        let provider = Arc::new(FakeProvider::new(vec![
            holiday_home,
            holiday_work,
            other_home,
            other_filename.clone(),
        ]));
        let engine = SearchEngine::new(provider.clone());

        let page = engine
            .search(request("(tags:home OR path:Holiday) AND filename:clip"))
            .unwrap();
        let mut found = names(&page);
        found.sort();
        assert_eq!(found, ["clip [home].mp4", "clip [work].mp4"]);
        assert_eq!(*provider.whole_path_asks.lock().unwrap(), [true]);

        let filename_only = engine.search(request("filename:home")).unwrap();
        assert_eq!(names(&filename_only), ["home clip.mp4"]);
    }

    #[test]
    fn not_binds_tighter_than_and_and_or() {
        let directory = tempdir().unwrap();
        let summer_draft = directory.path().join("summer draft.mp4");
        let winter_final = directory.path().join("winter final.mp4");
        let winter_draft = directory.path().join("winter draft.mp4");
        for path in [&summer_draft, &winter_final, &winter_draft] {
            fs::write(path, b"test").unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![
            summer_draft,
            winter_final,
            winter_draft,
        ])));

        let mut found = names(
            &engine
                .search(request(
                    "filename:summer OR filename:winter AND NOT filename:draft",
                ))
                .unwrap(),
        );
        found.sort();
        assert_eq!(found, ["summer draft.mp4", "winter final.mp4"]);
    }

    #[test]
    fn type_extension_and_folder_qualifiers_match_exact_metadata() {
        let directory = tempdir().unwrap();
        let archive = directory.path().join("Family Archive");
        fs::create_dir(&archive).unwrap();
        let photo = archive.join("Trip.JPEG");
        let video = archive.join("Trip.mp4");
        for path in [&photo, &video] {
            fs::write(path, b"test").unwrap();
        }
        let provider = Arc::new(FakeProvider::new(vec![photo, video]));
        let engine = SearchEngine::new(provider.clone());
        let both = |query: &str| SearchRequest {
            media_type: MediaType::Both,
            ..request(query)
        };

        assert_eq!(
            names(
                &engine
                    .search(both("type:image extension:jpeg folder:\"Family Archive\"",))
                    .unwrap(),
            ),
            ["Trip.JPEG"]
        );
        assert_eq!(
            names(
                &engine
                    .search(both("type:video extension:mp4 folder:archive"))
                    .unwrap(),
            ),
            ["Trip.mp4"]
        );
        assert_eq!(*provider.whole_path_asks.lock().unwrap(), [true, true]);
    }

    #[test]
    fn quoted_values_keep_spaces_and_escaped_quotes() {
        let directory = tempdir().unwrap();
        let phrase = directory.path().join("Family Trip.mp4");
        let quote = directory.path().join("Family \"Trip\".mp4");
        for path in [&phrase, &quote] {
            fs::write(path, b"test").unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![phrase, quote])));

        assert_eq!(
            names(&engine.search(request("filename:\"family trip\"")).unwrap(),),
            ["Family Trip.mp4"]
        );
        assert_eq!(
            names(
                &engine
                    .search(request("filename:\"family \\\"trip\\\"\""))
                    .unwrap(),
            ),
            ["Family \"Trip\".mp4"]
        );
    }

    #[test]
    fn relative_dates_resolve_once_from_the_search_date() {
        let today = CalendarDate::new(2026, 3, 1).unwrap();

        assert_eq!(
            parse_query_at("modified:>\"3 days ago\"", today).unwrap(),
            QueryExpr::Term(QueryTerm::Date {
                field: DateField::Modified,
                comparison: Comparison::Greater,
                date: CalendarDate::new(2026, 2, 26).unwrap(),
            })
        );
        assert_eq!(
            parse_query_at("created:>=\"1 week ago\"", today).unwrap(),
            QueryExpr::Term(QueryTerm::Date {
                field: DateField::Created,
                comparison: Comparison::GreaterOrEqual,
                date: CalendarDate::new(2026, 2, 22).unwrap(),
            })
        );
        assert_eq!(
            subtract_calendar_days(CalendarDate::new(2024, 3, 1).unwrap(), 1).unwrap(),
            CalendarDate::new(2024, 2, 29).unwrap()
        );
        assert_eq!(
            subtract_calendar_days(CalendarDate::new(2026, 1, 1).unwrap(), 1).unwrap(),
            CalendarDate::new(2025, 12, 31).unwrap()
        );
    }

    #[test]
    fn unknown_field_names_remain_plain_text() {
        let directory = tempdir().unwrap();
        let matching = directory.path().join("camera:model.mp4");
        fs::write(&matching, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![matching])));

        assert_eq!(
            names(&engine.search(request("camera:model")).unwrap()),
            ["camera:model.mp4"]
        );
    }

    #[test]
    fn new_operators_reject_missing_or_unknown_values() {
        for invalid in [
            "NOT",
            "type:document",
            "extension:",
            "folder:",
            "modified:\"soon\"",
            "filename:\"unfinished",
        ] {
            assert!(
                parse_query(invalid).is_err(),
                "expected {invalid} to be rejected"
            );
        }
    }

    #[test]
    fn advanced_search_rejects_incomplete_expressions() {
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(Vec::new())));

        assert!(matches!(
            engine.search(request("(clip OR movie")),
            Err(SearchError::InvalidQuery)
        ));
        assert!(matches!(
            engine.search(request("clip AND")),
            Err(SearchError::InvalidQuery)
        ));
        assert!(matches!(
            engine.search(request("AND clip")),
            Err(SearchError::InvalidQuery)
        ));
    }

    #[test]
    fn size_predicates_match_exact_sizes_and_ranges() {
        let directory = tempdir().unwrap();
        let small = directory.path().join("small.mp4");
        let target = directory.path().join("target.mp4");
        let huge = directory.path().join("huge.mp4");
        for (path, size) in [
            (&small, 100_000),
            (&target, 36_300_000),
            (&huge, 1_600_000_000),
        ] {
            fs::File::create(path).unwrap().set_len(size).unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![small, target, huge])));

        assert_eq!(
            names(&engine.search(request("size:36.3mb")).unwrap()),
            ["target.mp4"]
        );
        let mut under = names(&engine.search(request("size:<1.5gb")).unwrap());
        under.sort();
        assert_eq!(under, ["small.mp4", "target.mp4"]);
        let mut at_least = names(&engine.search(request("size:>=100kb")).unwrap());
        at_least.sort();
        assert_eq!(at_least, ["huge.mp4", "small.mp4", "target.mp4"]);
    }

    #[test]
    fn date_predicates_match_created_and_modified_dates() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first.mp4");
        let second = directory.path().join("second.mp4");
        fs::write(&first, b"test").unwrap();
        fs::write(&second, b"test").unwrap();
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(vec![first, second])));

        let mut modified = names(&engine.search(request("modified:>=01-01-1970")).unwrap());
        modified.sort();
        assert_eq!(modified, ["first.mp4", "second.mp4"]);

        let date = calendar_date(
            fs::metadata(directory.path().join("first.mp4"))
                .unwrap()
                .modified()
                .unwrap(),
        )
        .unwrap();
        let exact_query = format!(
            "modified:{:04}-{:02}-{:02}",
            date.year, date.month, date.day
        );
        let mut exact = names(&engine.search(request(&exact_query)).unwrap());
        exact.sort();
        assert_eq!(exact, ["first.mp4", "second.mp4"]);

        let mut created = names(&engine.search(request("created:>01-01-1970")).unwrap());
        created.sort();
        assert_eq!(created, ["first.mp4", "second.mp4"]);

        assert!(matches!(
            engine.search(request("modified:2026-02-30")),
            Err(SearchError::InvalidQuery)
        ));
    }

    #[test]
    fn date_parser_accepts_the_supported_formats_and_validates_calendar_dates() {
        assert_eq!(
            parse_calendar_date("10-5-2026").unwrap(),
            CalendarDate {
                year: 2026,
                month: 10,
                day: 5,
            }
        );
        assert_eq!(
            parse_calendar_date("10-5-26").unwrap(),
            CalendarDate {
                year: 2026,
                month: 10,
                day: 5,
            }
        );
        assert_eq!(
            parse_calendar_date("2026-10-05").unwrap(),
            CalendarDate {
                year: 2026,
                month: 10,
                day: 5,
            }
        );
        assert!(parse_calendar_date("02-29-2024").is_ok());
        for invalid in ["02-29-2023", "04-31-2026", "13-1-2026", "2026-1-1-1"] {
            assert!(
                parse_calendar_date(invalid).is_err(),
                "expected {invalid} to be rejected"
            );
        }
    }

    #[test]
    fn a_search_with_nothing_selected_is_refused() {
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(Vec::new())));

        let error = engine
            .search(SearchRequest {
                fields: SearchFields {
                    tags: false,
                    file_name: false,
                    path: false,
                },
                ..request("clip")
            })
            .unwrap_err();

        assert!(matches!(error, SearchError::NoSearchFields), "{error}");
    }

    #[test]
    fn a_renamed_video_is_found_under_its_new_name_while_the_index_is_stale() {
        let directory = tempdir().unwrap();
        let indexed = directory.path().join("sample1.mp4");
        fs::write(&indexed, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![indexed.clone()]));
        let engine = SearchEngine::new(provider.clone());
        let first = engine.search(request("sample")).unwrap();

        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&indexed, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged.clone())
            .unwrap();

        // The index still lists the name the file had before it was tagged.
        let stale = engine.search(request("sample")).unwrap();
        assert_eq!(stale.total_results, 1);
        assert_eq!(stale.results[0].file_name, "sample1 [home].mp4");
        assert_eq!(stale.results[0].tags, ["home"]);

        // The index has since dropped the name, because the file behind it is
        // gone; the renamed file must not disappear with it.
        provider.paths.lock().unwrap().clear();
        let dropped = engine.search(request("sample")).unwrap();
        assert_eq!(dropped.total_results, 1);
        assert_eq!(dropped.results[0].file_name, "sample1 [home].mp4");

        // Once the index catches up the file is listed once, not twice.
        *provider.paths.lock().unwrap() = vec![tagged];
        let refreshed = engine.search(request("sample")).unwrap();
        assert_eq!(refreshed.total_results, 1);
    }

    #[test]
    fn a_tagged_video_stays_searchable_after_restart_before_the_index_catches_up() {
        let directory = tempdir().unwrap();
        let indexed = directory.path().join("sample1.mp4");
        fs::write(&indexed, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![indexed.clone()]));
        let rename_store = directory.path().join("state/search-renames.json");
        let new_engine = || {
            SearchEngine::new_with_logger_and_rename_store(
                provider.clone(),
                Arc::new(SearchLogger::disabled()),
                Some(rename_store.clone()),
            )
        };
        let engine = new_engine();
        let first = engine.search(request("sample")).unwrap();

        let tagged = tags::add(&indexed, &["home".into()]).unwrap().path;
        engine
            .update_video_path(&first.results[0].id, tagged.clone())
            .unwrap();

        // Model the managed plocate provider dropping the stale path with
        // --existing until the background indexer has rebuilt its database.
        provider.paths.lock().unwrap().clear();
        drop(engine);

        let restarted = new_engine();
        let page = restarted.search(request("home")).unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "sample1 [home].mp4");
    }

    #[test]
    fn open_search_engines_keep_each_others_pending_renames() {
        let directory = tempdir().unwrap();
        let indexed = [
            directory.path().join("clip-one.mp4"),
            directory.path().join("clip-two.mp4"),
        ];
        for path in &indexed {
            fs::write(path, b"test").unwrap();
        }
        let provider_one = Arc::new(FakeProvider::new(indexed.to_vec()));
        let provider_two = Arc::new(FakeProvider::new(indexed.to_vec()));
        let rename_store = directory.path().join("state/search-renames.json");
        let new_engine = |provider| {
            SearchEngine::new_with_logger_and_rename_store(
                provider,
                Arc::new(SearchLogger::disabled()),
                Some(rename_store.clone()),
            )
        };
        let first_engine = new_engine(provider_one.clone());
        let second_engine = new_engine(provider_two.clone());
        let first_ids = first_engine.search(request("clip")).unwrap();
        let second_ids = second_engine.search(request("clip")).unwrap();

        let first_id = first_ids
            .results
            .iter()
            .find(|result| result.file_name == "clip-one.mp4")
            .unwrap()
            .id
            .clone();
        let second_id = second_ids
            .results
            .iter()
            .find(|result| result.file_name == "clip-two.mp4")
            .unwrap()
            .id
            .clone();
        let tagged_one = tags::add(&indexed[0], &["home".into()]).unwrap().path;
        first_engine
            .update_video_path(&first_id, tagged_one)
            .unwrap();
        let tagged_two = tags::add(&indexed[1], &["home".into()]).unwrap().path;
        second_engine
            .update_video_path(&second_id, tagged_two)
            .unwrap();

        provider_one.paths.lock().unwrap().clear();
        provider_two.paths.lock().unwrap().clear();
        let visible_to_first = first_engine.search(request("home")).unwrap();
        assert_eq!(visible_to_first.total_results, 2);

        let restarted = new_engine(provider_one);
        let after_restart = restarted.search(request("home")).unwrap();
        assert_eq!(after_restart.total_results, 2);
    }

    #[cfg(unix)]
    #[test]
    fn rename_overlay_lock_waits_for_another_engine_write() {
        use std::{sync::mpsc, thread, time::Duration};

        let directory = tempdir().unwrap();
        let rename_store = directory.path().join("state/search-renames.json");
        let held_lock = lock_rename_overlay(&rename_store).unwrap();
        let path = rename_store.clone();
        let (started, started_rx) = mpsc::channel();
        let (acquired, acquired_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            started.send(()).unwrap();
            let _lock = lock_rename_overlay(&path).unwrap();
            acquired.send(()).unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(acquired_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(held_lock);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        waiter.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saved_rename_state_preserves_non_unicode_path_bytes() {
        use std::{collections::HashMap, ffi::OsString, os::unix::ffi::OsStringExt};

        let indexed = PathBuf::from(OsString::from_vec(b"/videos-\xff/clip.mp4".to_vec()));
        let current = PathBuf::from(OsString::from_vec(b"/videos-\xff/clip [home].mp4".to_vec()));
        let files = HashMap::from([(indexed, current)]);
        let folders = HashMap::new();
        let overlay = PersistedRenameOverlay::from_maps(&files, &folders);

        let bytes = serde_json::to_vec(&overlay).unwrap();
        let restored: PersistedRenameOverlay = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(restored.into_maps(), (files, folders));
    }

    #[test]
    fn a_renamed_video_is_only_found_by_terms_matching_its_new_name() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sample1.mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![video.clone()]));
        let engine = SearchEngine::new(provider.clone());
        let first = engine.search(request("sample")).unwrap();
        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&video, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged)
            .unwrap();
        provider.paths.lock().unwrap().clear();

        let page = engine.search(request("holiday")).unwrap();

        assert_eq!(page.total_results, 0);
    }

    #[test]
    fn a_video_renamed_back_to_its_indexed_name_is_listed_once() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sample1.mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![video.clone()]));
        let engine = SearchEngine::new(provider);
        let request = || request("sample");
        let first = engine.search(request()).unwrap();

        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&video, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged.clone())
            .unwrap();
        let tagged_page = engine.search(request()).unwrap();
        fs::rename(&tagged, &video).unwrap();
        engine
            .update_video_path(&tagged_page.results[0].id, video)
            .unwrap();

        let page = engine.search(request()).unwrap();

        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].file_name, "sample1.mp4");
    }

    #[test]
    fn a_renamed_video_that_is_gone_is_not_listed() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sample1.mp4");
        fs::write(&video, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![video.clone()]));
        let engine = SearchEngine::new(provider.clone());
        let first = engine.search(request("sample")).unwrap();
        let tagged = directory.path().join("sample1 [home].mp4");
        fs::rename(&video, &tagged).unwrap();
        engine
            .update_video_path(&first.results[0].id, tagged.clone())
            .unwrap();
        provider.paths.lock().unwrap().clear();
        fs::remove_file(&tagged).unwrap();

        let page = engine.search(request("sample")).unwrap();

        assert_eq!(page.total_results, 0);
    }

    /// A folder holding `names`, inside `parent`, with every file written.
    fn folder_of(parent: &Path, folder: &str, names: &[&str]) -> Vec<PathBuf> {
        let folder = parent.join(folder);
        fs::create_dir_all(&folder).unwrap();
        names
            .iter()
            .map(|name| {
                let path = folder.join(name);
                fs::write(&path, b"test").unwrap();
                path
            })
            .collect()
    }

    fn id_of(page: &SearchPage, file_name: &str) -> String {
        page.results
            .iter()
            .find(|result| result.file_name == file_name)
            .unwrap()
            .id
            .clone()
    }

    fn file_names(page: &SearchPage) -> Vec<String> {
        let mut names: Vec<String> = page
            .results
            .iter()
            .map(|result| result.file_name.clone())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_result_carries_the_tags_of_its_folder() {
        let directory = tempdir().unwrap();
        let videos = folder_of(directory.path(), "Trip [beach]", &["clip.mp4"]);
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(videos)));

        let page = engine.search(request("clip")).unwrap();

        assert_eq!(page.results[0].folder_tags, ["beach"]);
        assert!(page.results[0].tags.is_empty());
    }

    #[test]
    fn a_renamed_folder_moves_every_video_the_engine_knows_in_it() {
        let directory = tempdir().unwrap();
        let mut videos = folder_of(directory.path(), "Trip", &["clip-a.mp4", "clip-b.mp4"]);
        let outside = folder_of(directory.path(), "Other", &["clip-c.mp4"]);
        videos.extend(outside);
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(videos)));
        let page = engine.search(request("clip")).unwrap();
        let (a, b, c) = (
            id_of(&page, "clip-a.mp4"),
            id_of(&page, "clip-b.mp4"),
            id_of(&page, "clip-c.mp4"),
        );

        let tagged = directory.path().join("Trip [beach]");
        fs::rename(directory.path().join("Trip"), &tagged).unwrap();
        let mut moved = engine.rename_folder(&directory.path().join("Trip"), tagged.clone());
        moved.sort();

        let mut expected = vec![a.clone(), b.clone()];
        expected.sort();
        assert_eq!(moved, expected);
        assert_eq!(engine.video_path(&a).unwrap(), tagged.join("clip-a.mp4"));
        assert_eq!(engine.video_path(&b).unwrap(), tagged.join("clip-b.mp4"));
        assert!(engine.video_path(&c).is_ok());
    }

    #[test]
    fn videos_in_a_renamed_folder_are_found_while_the_index_is_stale() {
        let directory = tempdir().unwrap();
        let videos = folder_of(directory.path(), "Trip", &["clip-a.mp4", "clip-b.mp4"]);
        let provider = Arc::new(FakeProvider::new(videos));
        let engine = SearchEngine::new(provider.clone());
        engine.search(request("clip")).unwrap();
        let folder = directory.path().join("Trip");
        let tagged = directory.path().join("Trip [beach]");
        fs::rename(&folder, &tagged).unwrap();
        engine.rename_folder(&folder, tagged.clone());

        // The index still lists the videos under the folder's old name.
        let stale = engine.search(request("clip")).unwrap();
        assert_eq!(stale.total_results, 2);
        assert!(stale.results.iter().all(|r| r.folder_tags == ["beach"]));

        // The index has dropped them, because nothing is at the old name now.
        provider.paths.lock().unwrap().clear();
        let dropped = engine.search(request("clip")).unwrap();
        assert_eq!(file_names(&dropped), ["clip-a.mp4", "clip-b.mp4"]);

        // Once it catches up, each video is listed once.
        *provider.paths.lock().unwrap() =
            vec![tagged.join("clip-a.mp4"), tagged.join("clip-b.mp4")];
        let refreshed = engine.search(request("clip")).unwrap();
        assert_eq!(refreshed.total_results, 2);

        // And a folder tag is something a search can find.
        let by_tag = engine.search(request("beach")).unwrap();
        assert_eq!(by_tag.total_results, 2);
    }

    #[test]
    fn a_video_tagged_before_its_folder_follows_the_folder() {
        let directory = tempdir().unwrap();
        let videos = folder_of(directory.path(), "Trip", &["clip.mp4"]);
        let provider = Arc::new(FakeProvider::new(videos.clone()));
        let engine = SearchEngine::new(provider);
        let page = engine.search(request("clip")).unwrap();
        let id = page.results[0].id.clone();
        let folder = directory.path().join("Trip");

        let tagged_video = folder.join("clip [home].mp4");
        fs::rename(&videos[0], &tagged_video).unwrap();
        engine.update_video_path(&id, tagged_video).unwrap();
        let tagged_folder = directory.path().join("Trip [beach]");
        fs::rename(&folder, &tagged_folder).unwrap();
        engine.rename_folder(&folder, tagged_folder.clone());

        let stale = engine.search(request("clip")).unwrap();
        assert_eq!(stale.total_results, 1);
        assert_eq!(stale.results[0].file_name, "clip [home].mp4");
        assert_eq!(stale.results[0].folder_tags, ["beach"]);
        assert_eq!(
            engine.video_path(&id).unwrap(),
            tagged_folder.join("clip [home].mp4")
        );
    }

    #[test]
    fn a_video_tagged_after_its_folder_is_found_under_both_names() {
        let directory = tempdir().unwrap();
        let videos = folder_of(directory.path(), "Trip", &["clip.mp4"]);
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(videos)));
        engine.search(request("clip")).unwrap();
        let folder = directory.path().join("Trip");
        let tagged_folder = directory.path().join("Trip [beach]");
        fs::rename(&folder, &tagged_folder).unwrap();
        engine.rename_folder(&folder, tagged_folder.clone());
        let page = engine.search(request("clip")).unwrap();

        let tagged_video = tagged_folder.join("clip [home].mp4");
        fs::rename(tagged_folder.join("clip.mp4"), &tagged_video).unwrap();
        engine
            .update_video_path(&page.results[0].id, tagged_video)
            .unwrap();

        let stale = engine.search(request("clip")).unwrap();
        assert_eq!(stale.total_results, 1);
        assert_eq!(stale.results[0].file_name, "clip [home].mp4");
        assert_eq!(stale.results[0].tags, ["home"]);
        assert_eq!(stale.results[0].folder_tags, ["beach"]);
    }

    #[test]
    fn a_folder_renamed_back_to_its_indexed_name_lists_each_video_once() {
        let directory = tempdir().unwrap();
        let videos = folder_of(directory.path(), "Trip", &["clip.mp4"]);
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(videos)));
        engine.search(request("clip")).unwrap();
        let folder = directory.path().join("Trip");
        let tagged = directory.path().join("Trip [beach]");
        fs::rename(&folder, &tagged).unwrap();
        engine.rename_folder(&folder, tagged.clone());
        fs::rename(&tagged, &folder).unwrap();
        engine.rename_folder(&tagged, folder.clone());

        let page = engine.search(request("clip")).unwrap();

        assert_eq!(page.total_results, 1);
        assert!(page.results[0].folder_tags.is_empty());
    }

    #[test]
    fn videos_handed_to_the_engine_become_a_playable_page_in_the_order_given() {
        let directory = tempdir().unwrap();
        let beach = directory.path().join("Beach day [home].mp4");
        let party = directory.path().join("party.MKV");
        for path in [&beach, &party] {
            fs::write(path, b"test").unwrap();
        }
        let engine = SearchEngine::new(Arc::new(FakeProvider::new(Vec::new())));

        let page = engine.page_of_videos("summer.m3u8".into(), vec![party.clone(), beach.clone()]);

        assert_eq!(page.query, "summer.m3u8");
        assert_eq!(page.total_results, 2);
        assert_eq!(page.total_pages, 1);
        // A playlist's order is the order it asked to be played in, so these
        // are not sorted by name the way a search's results are.
        assert_eq!(page.results[0].file_name, "party.MKV");
        assert_eq!(page.results[0].extension, "mkv");
        assert_eq!(page.results[1].file_name, "Beach day [home].mp4");
        assert_eq!(page.results[1].tags, ["home"]);
        // And each result plays the video it stands for.
        assert_eq!(engine.video_path(&page.results[0].id).unwrap(), party);
        assert_eq!(engine.video_path(&page.results[1].id).unwrap(), beach);
    }

    #[test]
    fn a_result_id_remains_valid_after_a_later_search() {
        let directory = tempdir().unwrap();
        let first_path = directory.path().join("first-clip.mp4");
        let second_path = directory.path().join("second-clip.mp4");
        fs::write(&first_path, b"test").unwrap();
        fs::write(&second_path, b"test").unwrap();
        let provider = Arc::new(FakeProvider::new(vec![first_path.clone()]));
        let engine = SearchEngine::new(provider.clone());

        let first_page = engine.search(request("first")).unwrap();
        *provider.paths.lock().unwrap() = vec![second_path];
        engine.search(request("second")).unwrap();

        assert_eq!(
            engine.video_path(&first_page.results[0].id).unwrap(),
            first_path
        );
    }

    #[test]
    fn search_defaults_to_videos_only() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sunset.mp4");
        let image = directory.path().join("sunset.jpg");
        for path in [&video, &image] {
            fs::write(path, b"test").unwrap();
        }
        let provider = Arc::new(FakeProvider::new(vec![video.clone(), image.clone()]));
        let engine = SearchEngine::new(provider);
        let page = engine.search(request("sunset")).unwrap();
        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].extension, "mp4");
    }

    #[test]
    fn search_with_images_only_returns_images() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sunset.mp4");
        let image = directory.path().join("sunset.jpg");
        for path in [&video, &image] {
            fs::write(path, b"test").unwrap();
        }
        let provider = Arc::new(FakeProvider::new(vec![video, image.clone()]));
        let engine = SearchEngine::new(provider);
        let mut req = request("sunset");
        req.media_type = MediaType::Images;
        let page = engine.search(req).unwrap();
        assert_eq!(page.total_results, 1);
        assert_eq!(page.results[0].extension, "jpg");
    }

    #[test]
    fn search_with_both_returns_videos_and_images() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("clip.mp4");
        let image = directory.path().join("clip.jpg");
        for path in [&video, &image] {
            fs::write(path, b"test").unwrap();
        }
        let provider = Arc::new(FakeProvider::new(vec![video, image]));
        let engine = SearchEngine::new(provider);
        let mut req = request("clip");
        req.media_type = MediaType::Both;
        let page = engine.search(req).unwrap();
        assert_eq!(page.total_results, 2);
    }

    #[test]
    fn is_supported_image_recognises_common_extensions() {
        assert!(is_supported_image(Path::new("photo.jpg")));
        assert!(is_supported_image(Path::new("photo.JPEG")));
        assert!(is_supported_image(Path::new("photo.png")));
        assert!(is_supported_image(Path::new("photo.webp")));
        assert!(is_supported_image(Path::new("photo.heic")));
        assert!(!is_supported_image(Path::new("photo.mp4")));
        assert!(!is_supported_image(Path::new("photo.txt")));
    }

    #[test]
    fn is_supported_media_respects_filter() {
        assert!(is_supported_media(Path::new("clip.mp4"), MediaType::Videos));
        assert!(!is_supported_media(
            Path::new("clip.jpg"),
            MediaType::Videos
        ));
        assert!(is_supported_media(Path::new("clip.jpg"), MediaType::Images));
        assert!(!is_supported_media(
            Path::new("clip.mp4"),
            MediaType::Images
        ));
        assert!(is_supported_media(Path::new("clip.mp4"), MediaType::Both));
        assert!(is_supported_media(Path::new("clip.jpg"), MediaType::Both));
    }
}
