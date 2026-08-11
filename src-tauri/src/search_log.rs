use crate::search::{MediaType, SearchFields, SearchRequest};
use serde_json::{json, Value};
use std::{
    fs::{create_dir_all, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchLogContext {
    pub(crate) query: String,
    pub(crate) page: usize,
    pub(crate) page_size: usize,
    pub(crate) fields: SearchFields,
    pub(crate) media_type: MediaType,
}

impl From<&SearchRequest> for SearchLogContext {
    fn from(request: &SearchRequest) -> Self {
        Self {
            query: request.query.clone(),
            page: request.page,
            page_size: request.page_size,
            fields: request.fields,
            media_type: request.media_type,
        }
    }
}

pub(crate) struct SearchLogger {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl SearchLogger {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self::new(PathBuf::new())
    }

    pub(crate) fn system() -> Self {
        #[cfg(target_os = "macos")]
        let path = dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Library/Logs/app.toka.desktop/search.log");
        #[cfg(not(target_os = "macos"))]
        let path = dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("app.toka.desktop/logs/search.log");
        Self::new(path)
    }

    pub(crate) fn record_command(
        &self,
        context: &SearchLogContext,
        program: &str,
        args: &[String],
        success: bool,
        exit_code: Option<i32>,
        stderr: &str,
    ) {
        let mut entry = context_entry(context, "command");
        entry.insert("program".into(), json!(program));
        entry.insert("args".into(), json!(args));
        entry.insert("command".into(), json!(display_command(program, args)));
        entry.insert("success".into(), json!(success));
        entry.insert("exitCode".into(), json!(exit_code));
        if !stderr.trim().is_empty() {
            entry.insert("stderr".into(), json!(stderr.trim()));
        }
        self.write(Value::Object(entry));
    }

    pub(crate) fn record_error(&self, context: &SearchLogContext, error: &str) {
        let mut entry = context_entry(context, "error");
        entry.insert("error".into(), json!(error));
        self.write(Value::Object(entry));
    }

    fn write(&self, entry: Value) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        let _guard = self.write_lock.lock().unwrap();
        let Some(parent) = self.path.parent() else {
            return;
        };
        if let Err(error) = create_dir_all(parent) {
            eprintln!("Could not create Toka search log directory: {error}");
            return;
        }
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            eprintln!("Could not open Toka search log: {}", self.path.display());
            return;
        };
        let Ok(line) = serde_json::to_string(&entry) else {
            eprintln!("Could not serialize a Toka search log entry");
            return;
        };
        if let Err(error) = writeln!(file, "{line}") {
            eprintln!("Could not write Toka search log: {error}");
        }
    }
}

fn context_entry(context: &SearchLogContext, kind: &str) -> serde_json::Map<String, Value> {
    let mut entry = serde_json::Map::new();
    entry.insert("kind".into(), json!(kind));
    entry.insert(
        "timestamp".into(),
        json!(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()),
    );
    entry.insert("query".into(), json!(context.query));
    entry.insert("page".into(), json!(context.page));
    entry.insert("pageSize".into(), json!(context.page_size));
    entry.insert(
        "fields".into(),
        json!({
            "tags": context.fields.tags,
            "fileName": context.fields.file_name,
            "path": context.fields.path,
        }),
    );
    entry.insert("mediaType".into(), json!(context.media_type));
    entry
}

fn display_command(program: &str, args: &[String]) -> String {
    std::iter::once(program)
        .chain(args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-._/:@".contains(character))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn records_search_parameters_and_the_exact_command() {
        let directory = tempfile::tempdir().unwrap();
        let logger = SearchLogger::new(directory.path().join("search.log"));
        let context = SearchLogContext {
            query: "summer vacation".into(),
            page: 1,
            page_size: 24,
            fields: SearchFields {
                tags: true,
                file_name: true,
                path: false,
            },
            media_type: MediaType::Videos,
        };

        logger.record_command(
            &context,
            "plocate",
            &[
                "--ignore-case".into(),
                "--basename".into(),
                "--existing".into(),
                "--".into(),
                "vacation".into(),
            ],
            true,
            Some(0),
            "",
        );

        let line = std::fs::read_to_string(directory.path().join("search.log")).unwrap();
        let entry: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(entry["kind"], "command");
        assert_eq!(entry["query"], "summer vacation");
        assert_eq!(entry["page"], 1);
        assert_eq!(entry["pageSize"], 24);
        assert_eq!(entry["fields"]["tags"], true);
        assert_eq!(entry["fields"]["fileName"], true);
        assert_eq!(entry["fields"]["path"], false);
        assert_eq!(entry["mediaType"], "videos");
        assert_eq!(entry["program"], "plocate");
        assert_eq!(
            entry["command"],
            "plocate --ignore-case --basename --existing -- vacation"
        );
        assert_eq!(entry["exitCode"], 0);
        assert!(entry["timestamp"].as_u64().is_some());
    }

    #[test]
    fn records_search_errors_without_failing_the_caller() {
        let directory = tempfile::tempdir().unwrap();
        let logger = SearchLogger::new(directory.path().join("search.log"));
        let context = SearchLogContext {
            query: "broken".into(),
            page: 1,
            page_size: 24,
            fields: SearchFields::default(),
            media_type: MediaType::Videos,
        };

        logger.record_error(&context, "plocate failed");

        let line = std::fs::read_to_string(directory.path().join("search.log")).unwrap();
        let entry: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(entry["kind"], "error");
        assert_eq!(entry["query"], "broken");
        assert_eq!(entry["error"], "plocate failed");
    }
}
