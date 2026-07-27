use std::path::Path;
use std::process::Command;

pub fn get(path: &Path) -> Vec<String> {
    Command::new("tag")
        .args(["get", path.to_string_lossy().as_ref()])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|output| output.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_default()
}

pub fn set(path: &Path, tags: &[String]) -> Result<Vec<String>, String> {
    let output = Command::new("tag")
        .arg("set")
        .arg(path)
        .arg("--")
        .args(tags)
        .output()
        .map_err(|error| format!("The tag command could not be started: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(get(path))
}
