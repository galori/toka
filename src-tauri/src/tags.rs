use std::path::{Path, PathBuf};
use std::process::Command;

pub struct TagUpdate {
    pub path: PathBuf,
    pub tags: Vec<String>,
}

pub fn get(path: &Path) -> Vec<String> {
    get_with_command(&tag_command(), path)
}

fn tag_command() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local/bin/tag"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("tag"))
}

fn get_with_command(command: &Path, path: &Path) -> Vec<String> {
    Command::new(command)
        .args(["get", path.to_string_lossy().as_ref()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

pub fn set(path: &Path, tags: &[String]) -> Result<TagUpdate, String> {
    set_with_command(&tag_command(), path, tags)
}

fn set_with_command(command: &Path, path: &Path, tags: &[String]) -> Result<TagUpdate, String> {
    let output = Command::new(command)
        .arg("set")
        .arg(path)
        .arg("--")
        .args(tags)
        .output()
        .map_err(|error| format!("The tag command could not be started: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let renamed_path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| PathBuf::from(line.trim()))
        .ok_or_else(|| "The tag command did not report the renamed video path.".to_owned())?;
    Ok(TagUpdate {
        tags: get_with_command(command, &renamed_path),
        path: renamed_path,
    })
}

#[cfg(test)]
mod tests {
    use super::set_with_command;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn set_reads_tags_from_the_renamed_file() {
        let directory = tempdir().unwrap();
        let video = directory.path().join("sample.mp4");
        let command = directory.path().join("tag");
        fs::write(&video, b"video").unwrap();
        fs::write(
            &command,
            "#!/bin/sh\nif [ \"$1\" = set ]; then\n  renamed=\"${2%.mp4} [funny].mp4\"\n  mv \"$2\" \"$renamed\"\n  printf '%s\\n' \"$renamed\"\nelif [ \"$1\" = get ]; then\n  printf funny\nfi\n",
        )
        .unwrap();
        fs::set_permissions(&command, fs::Permissions::from_mode(0o755)).unwrap();

        let update = set_with_command(&command, &video, &["funny".into()]).unwrap();
        assert_eq!(update.path, directory.path().join("sample [funny].mp4"));
        assert_eq!(update.tags, ["funny"]);
    }
}
