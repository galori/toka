use crate::addons::AddonSearchLaunch;
use std::{ffi::OsString, path::PathBuf};

pub const ADDON_SEARCH_FLAG: &str = "--toka-addon-search";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchIntent {
    Empty,
    Media(PathBuf),
    AddonSearch(AddonSearchLaunch),
}

pub fn from_arguments(
    arguments: impl IntoIterator<Item = OsString>,
) -> Result<LaunchIntent, String> {
    let arguments = arguments.into_iter().collect::<Vec<_>>();
    if arguments
        .get(1)
        .is_some_and(|argument| argument == ADDON_SEARCH_FLAG)
    {
        if arguments.len() != 3 {
            return Err("The add-on search launch is incomplete.".into());
        }
        let encoded = arguments[2]
            .to_str()
            .ok_or_else(|| "The add-on search launch is not valid text.".to_owned())?;
        let search = serde_json::from_str(encoded)
            .map_err(|error| format!("The add-on search launch is invalid: {error}"))?;
        return Ok(LaunchIntent::AddonSearch(search));
    }
    Ok(match crate::playlist::from_arguments(arguments) {
        Some(path) => LaunchIntent::Media(path),
        None => LaunchIntent::Empty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{MediaType, SearchFields};

    fn arguments(arguments: impl IntoIterator<Item = String>) -> Vec<OsString> {
        arguments.into_iter().map(OsString::from).collect()
    }

    #[test]
    fn addon_search_launch_round_trips_through_one_process_argument() {
        let search = AddonSearchLaunch {
            query: "@sometiktoker".into(),
            fields: SearchFields {
                tags: false,
                file_name: true,
                path: false,
            },
            media_type: MediaType::Videos,
            addon_id: "tiktok".into(),
            capture: "handle".into(),
            value: "@sometiktoker".into(),
        };
        let encoded = serde_json::to_string(&search).unwrap();

        assert_eq!(
            from_arguments(arguments([
                "toka".into(),
                ADDON_SEARCH_FLAG.into(),
                encoded,
            ]))
            .unwrap(),
            LaunchIntent::AddonSearch(search)
        );
    }
}
