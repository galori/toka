//! Lossy paths for JSON documents.
//!
//! Real media folders can hold non-UTF-8 names on Unix, and `PathBuf`'s own
//! serialization rejects those — inside `serde_json::json!` that rejection is
//! a panic. Every path a `--json` document carries therefore goes through
//! [`serialize`], which replaces invalid bytes exactly as the human-readable
//! output's `display()` does.

use serde::Serializer;
use std::path::{Path, PathBuf};

/// Serialize a path as lossy UTF-8, for `#[serde(serialize_with = ...)]`.
/// Valid UTF-8 serializes exactly as `PathBuf` would.
pub fn serialize<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&path.to_string_lossy())
}

/// Serialize an optional path the same way, for `#[serde(serialize_with =
/// ...)]` on `Option<PathBuf>`: `None` stays JSON null, never an empty
/// string a reader could mistake for a real path.
pub fn serialize_option<S>(path: &Option<PathBuf>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match path {
        Some(path) => serializer.serialize_some(&path.to_string_lossy()),
        None => serializer.serialize_none(),
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use std::path::PathBuf;

    #[derive(Serialize)]
    struct Document {
        #[serde(serialize_with = "super::serialize")]
        path: PathBuf,
    }

    #[test]
    fn valid_utf8_paths_serialize_unchanged() {
        let document = Document {
            path: PathBuf::from("/media/movies/family-trip.mp4"),
        };
        assert_eq!(
            serde_json::to_value(&document).unwrap()["path"],
            "/media/movies/family-trip.mp4"
        );
    }

    #[test]
    fn missing_optional_paths_serialize_as_null() {
        #[derive(Serialize)]
        struct MaybeDocument {
            #[serde(serialize_with = "super::serialize_option")]
            path: Option<PathBuf>,
        }

        let json = serde_json::to_value(MaybeDocument { path: None }).unwrap();
        assert!(json["path"].is_null());
        let json = serde_json::to_value(MaybeDocument {
            path: Some(PathBuf::from("/media/movies")),
        })
        .unwrap();
        assert_eq!(json["path"], "/media/movies");
    }

    #[test]
    #[cfg(unix)]
    fn non_utf8_paths_serialize_lossy_instead_of_failing() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let raw = OsString::from_vec(b"/media/movies/family-\xff-trip.mp4".to_vec());
        let document = Document {
            path: PathBuf::from(raw),
        };
        assert_eq!(
            serde_json::to_value(&document).unwrap()["path"],
            "/media/movies/family-�-trip.mp4"
        );
        // `PathBuf`'s own serialization rejects this path, which is what made
        // `json!` panic; the helper must never do that.
        assert!(serde_json::to_value(&document.path).is_err());
    }
}
