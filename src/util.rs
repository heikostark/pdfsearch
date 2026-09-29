use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Determines the directory the search index for a given PDF root
/// directory is stored in. Without an explicit override (`--index-dir`) a
/// stable path inside the user's system cache directory is used, derived
/// from the absolute source path. That way `index`, `search` and `serve`
/// automatically find the same index for the same folder again, without
/// index data ending up inside the watched directory itself.
pub fn resolve_index_dir(root: &Path, explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    let canonical = root
        .canonicalize()
        .with_context(|| format!("directory not found: {}", root.display()))?;

    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    let hash = hasher.finish();

    let base = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    let dirname = canonical
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());

    Ok(base.join("pdfsearch").join(format!("{dirname}-{hash:016x}")))
}

/// Base directory (inside the system cache dir) that holds pdfsearch's own
/// data that is not tied to a single indexed root, currently just the
/// registry of known directories used by the web GUI.
pub fn app_data_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("pdfsearch")
}
