use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn generate(video: &Path) -> Option<PathBuf> {
    let cache = std::env::temp_dir().join("toka-thumbnails");
    fs::create_dir_all(&cache).ok()?;
    let key = format!("{:x}", md5_fingerprint(video));
    let output = cache.join(format!("{key}.jpg"));
    if output.is_file() {
        return Some(output);
    }
    let temporary = output.with_extension("tmp.jpg");
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-ss", "1", "-i"])
        .arg(video)
        .args(["-frames:v", "1", "-vf", "scale=640:-2", "-q:v", "5", "-y"])
        .arg(&temporary)
        .status()
        .ok()?;
    if !status.success() || !temporary.is_file() {
        let _ = fs::remove_file(&temporary);
        return None;
    }
    fs::rename(&temporary, &output).ok()?;
    Some(output)
}

fn md5_fingerprint(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    if let Ok(metadata) = fs::metadata(path) {
        metadata.len().hash(&mut hasher);
        if let Ok(modified) = metadata.modified() {
            modified.hash(&mut hasher);
        }
    }
    hasher.finish()
}
