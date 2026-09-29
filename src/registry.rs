use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::util;

/// One directory the user has indexed at some point, together with where
/// its index lives. Kept so the web GUI's overview page can list every
/// known directory without having to guess index-directory names.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredRoot {
    pub root: PathBuf,
    pub index_dir: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    roots: Vec<RegisteredRoot>,
}

const REGISTRY_FILE: &str = "registry.json";

fn registry_path() -> Result<PathBuf> {
    let base = util::app_data_dir();
    fs::create_dir_all(&base)?;
    Ok(base.join(REGISTRY_FILE))
}

fn load_raw(path: &Path) -> Result<Registry> {
    if !path.exists() {
        return Ok(Registry::default());
    }
    let data = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&data).unwrap_or_default())
}

/// Adds (or updates) a directory in the registry. Called after every
/// successful `index` run, from both the CLI and the web GUI.
pub fn register(root: &Path, index_dir: &Path) -> Result<()> {
    let path = registry_path()?;
    let mut reg = load_raw(&path)?;
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    reg.roots.retain(|r| r.root != root_canon);
    reg.roots.push(RegisteredRoot {
        root: root_canon,
        index_dir: index_dir.to_path_buf(),
    });
    fs::write(&path, serde_json::to_string_pretty(&reg)?)?;
    Ok(())
}

/// Removes a directory from the registry (its index files are left
/// untouched on disk).
pub fn unregister(root: &Path) -> Result<()> {
    let path = registry_path()?;
    let mut reg = load_raw(&path)?;
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    reg.roots.retain(|r| r.root != root_canon);
    fs::write(&path, serde_json::to_string_pretty(&reg)?)?;
    Ok(())
}

/// Lists every directory registered so far.
pub fn list() -> Result<Vec<RegisteredRoot>> {
    let path = registry_path()?;
    Ok(load_raw(&path)?.roots)
}
