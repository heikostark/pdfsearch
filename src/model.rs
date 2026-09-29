use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Metadata used to detect whether a PDF file has changed since the last
/// indexing run, without having to re-read its contents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMeta {
    pub mtime_secs: u64,
    pub size: u64,
    pub pages: usize,
}

/// Persistent cache: path -> metadata. Stored next to the tantivy index
/// (cache.json) and enables incremental re-indexing: only new or changed
/// files are re-scanned via pdftotext, and files that were deleted from
/// disk are removed from the index again.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CacheStore {
    pub files: HashMap<String, FileMeta>,
}
