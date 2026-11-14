use std::{io, path::Path};

pub(crate) fn is_hidden(path: &Path) -> io::Result<bool> {
    let dotted = path
        .file_name()
        .is_some_and(|name| name.as_encoded_bytes().starts_with(b"."));
    #[cfg(target_os = "macos")]
    if !dotted {
        use std::os::macos::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(path)?;
        return Ok(metadata.is_dir() && metadata.st_flags() & libc::UF_HIDDEN != 0);
    }
    Ok(dotted)
}

pub(crate) fn hidden_ancestor<'a>(root: &Path, path: &'a Path) -> io::Result<Option<&'a Path>> {
    let ancestors = path
        .ancestors()
        .take_while(|ancestor| ancestor.starts_with(root))
        .collect::<Vec<_>>();
    for ancestor in ancestors.into_iter().rev() {
        match is_hidden(ancestor) {
            Ok(true) => return Ok(Some(ancestor)),
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}
