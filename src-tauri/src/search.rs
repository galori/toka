use crate::search_log::{SearchLogContext, SearchLogger};
use crate::tags;
use crate::thumbnails;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
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
    /// Files this session has renamed, indexed by the name the search provider
    /// knew them under. Every provider Toka uses answers from an index that is
    /// rebuilt on a schedule, so a file tagged a moment ago is still indexed
    /// under its old name — and an index that filters out names whose file is
    /// gone, as `plocate --existing` does, stops listing it at all. Keeping the
    /// renames here lets a search see the file the way it is now.
    renamed_paths: Mutex<HashMap<PathBuf, PathBuf>>,
    /// The order the search on screen is being read in. Only the current one is
    /// kept: a new question replaces it, and there is never a second search to
    /// go back to — the frontend asks for page one and then walks forward.
    shuffle: Mutex<Option<Shuffle>>,
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

    pub fn new_with_logger(provider: Arc<dyn SearchProvider>, logger: Arc<SearchLogger>) -> Self {
        Self {
            provider,
            logger,
            result_paths: Mutex::new(HashMap::new()),
            renamed_paths: Mutex::new(HashMap::new()),
            shuffle: Mutex::new(None),
        }
    }

    pub fn search(&self, request: SearchRequest) -> Result<SearchPage, SearchError> {
        let context = self.logger.context(&request, true);
        let result = self.search_page(request, &context);
        if let Err(error) = &result {
            self.logger.record_error(&context, &error.to_string());
        }
        result
    }

    fn search_page(
        &self,
        request: SearchRequest,
        context: &SearchLogContext,
    ) -> Result<SearchPage, SearchError> {
        let (query, mut paths) = self.matching_paths(&request, context)?;
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
        let context = self.logger.context(&request, false);
        let result = self
            .matching_paths(&request, &context)
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

    /// The provider's candidates, corrected for the renames this session has
    /// made: a stale name is replaced by the name its file carries now, and
    /// every renamed file the index has already dropped is added back. The
    /// caller still filters these by the search terms, so a rename only shows
    /// up when the query matches the name the file actually has.
    fn candidates(
        &self,
        query: &str,
        whole_path: bool,
        context: &SearchLogContext,
    ) -> Result<Vec<PathBuf>, SearchError> {
        let mut paths = self.provider.candidates(query, whole_path, context)?;
        let renamed = self.renamed_paths.lock().unwrap();
        if renamed.is_empty() {
            return Ok(paths);
        }
        for path in &mut paths {
            if let Some(current) = renamed.get(path) {
                *path = current.clone();
            }
        }
        // A renamed file that has since been deleted is not a search result,
        // and this is the only place a path Toka renamed itself can be checked
        // cheaply — provider candidates are deliberately left untouched.
        paths.extend(renamed.values().filter(|path| path.is_file()).cloned());
        Ok(paths)
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
        let mut renamed = self.renamed_paths.lock().unwrap();
        let indexed = renamed
            .iter()
            .find(|(_, path)| **path == previous)
            .map(|(indexed, _)| indexed.clone())
            .unwrap_or(previous);
        if indexed == current {
            renamed.remove(&indexed);
        } else {
            renamed.insert(indexed, current);
        }
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
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryField {
    Tags,
    FileName,
    Path,
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum QueryExpr {
    Term { field: QueryField, value: String },
    And(Box<QueryExpr>, Box<QueryExpr>),
    Or(Box<QueryExpr>, Box<QueryExpr>),
}

fn tokenize_query(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in query.chars() {
        if matches!(character, '(' | ')') {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            tokens.push(character.to_string());
        } else if character.is_whitespace() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_query(query: &str) -> Result<QueryExpr, SearchError> {
    let tokens = tokenize_query(query);
    if tokens.is_empty() {
        return Err(SearchError::InvalidQuery);
    }
    let mut parser = QueryParser {
        tokens,
        position: 0,
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
        let mut expression = self.parse_primary()?;
        loop {
            match self.peek() {
                None | Some(")") => break,
                Some(token) if token.eq_ignore_ascii_case("OR") => break,
                Some(token) if token.eq_ignore_ascii_case("AND") => {
                    self.consume();
                }
                Some(_) => {}
            }
            expression = QueryExpr::And(Box::new(expression), Box::new(self.parse_primary()?));
        }
        Ok(expression)
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
                if token.eq_ignore_ascii_case("AND") || token.eq_ignore_ascii_case("OR") =>
            {
                Err(SearchError::InvalidQuery)
            }
            Some(_) => Ok(parse_query_term(&self.consume().unwrap())),
        }
    }
}

fn parse_query_term(token: &str) -> QueryExpr {
    let (field, value) = token
        .split_once(':')
        .map(|(field, value)| (field.to_ascii_lowercase(), value))
        .and_then(|(field, value)| {
            let field = match field.as_str() {
                "tags" | "tag" => QueryField::Tags,
                "filename" | "file" | "name" => QueryField::FileName,
                "path" => QueryField::Path,
                _ => return None,
            };
            (!value.is_empty()).then_some((field, value))
        })
        .unwrap_or((QueryField::Any, token));
    QueryExpr::Term {
        field,
        value: value.to_lowercase(),
    }
}

fn query_needs_whole_path(expression: &QueryExpr) -> bool {
    match expression {
        QueryExpr::Term { field, .. } => *field == QueryField::Path,
        QueryExpr::And(left, right) | QueryExpr::Or(left, right) => {
            query_needs_whole_path(left) || query_needs_whole_path(right)
        }
    }
}

fn matches_query(path: &Path, expression: &QueryExpr, fields: SearchFields) -> bool {
    match expression {
        QueryExpr::Term { field, value } => match field {
            QueryField::Any => haystacks(path, fields)
                .iter()
                .any(|hay| hay.contains(value)),
            QueryField::Tags => tags_haystack(path).contains(value),
            QueryField::FileName => file_name_haystack(path).contains(value),
            QueryField::Path => path_haystack(path).contains(value),
        },
        QueryExpr::And(left, right) => {
            matches_query(path, left, fields) && matches_query(path, right, fields)
        }
        QueryExpr::Or(left, right) => {
            matches_query(path, left, fields) || matches_query(path, right, fields)
        }
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
