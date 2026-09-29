use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use crate::model::CacheStore;

const CACHE_FILE: &str = "cache.json";

pub fn load(index_dir: &Path) -> Result<CacheStore> {
    let path = index_dir.join(CACHE_FILE);
    if !path.exists() {
        return Ok(CacheStore::default());
    }
    let data = fs::read_to_string(&path)
        .with_context(|| format!("could not read cache file: {}", path.display()))?;
    let store: CacheStore = serde_json::from_str(&data).unwrap_or_default();
    Ok(store)
}

pub fn save(index_dir: &Path, store: &CacheStore) -> Result<()> {
    let path = index_dir.join(CACHE_FILE);
    let data = serde_json::to_string_pretty(store)?;
    fs::write(&path, data)
        .with_context(|| format!("could not write cache file: {}", path.display()))?;
    Ok(())
}
