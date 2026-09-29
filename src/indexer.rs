use anyhow::{Context, Result};
use indicatif::{ParallelProgressIterator, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use tantivy::directory::MmapDirectory;
use tantivy::{Index, Term};

use crate::cache;
use crate::model::{CacheStore, FileMeta};
use crate::schema::{self, Fields};
use crate::scan;

pub struct IndexStats {
    pub scanned: usize,
    pub updated: usize,
    pub removed: usize,
    pub failed: usize,
}

/// Opens an existing index or creates a new one.
pub fn open_or_create(index_dir: &Path) -> Result<(Index, Fields)> {
    std::fs::create_dir_all(index_dir)
        .with_context(|| format!("could not create index directory: {}", index_dir.display()))?;
    let (schema, fields) = schema::build();
    let dir = MmapDirectory::open(index_dir)
        .with_context(|| format!("index directory not usable: {}", index_dir.display()))?;
    let index = Index::open_or_create(dir, schema)
        .with_context(|| "could not open/create the tantivy index".to_string())?;
    Ok((index, fields))
}

/// Result of the (parallel) text extraction for a single file.
enum ScanResult {
    Unchanged,
    Updated {
        path: PathBuf,
        pages: Vec<String>,
        meta: FileMeta,
    },
    Failed {
        path: PathBuf,
        error: String,
    },
}

/// Recursively scans `root`, extracts text from new/changed PDFs in
/// parallel (rayon uses all CPU cores) and updates the index
/// incrementally: unchanged files are skipped, files that disappeared
/// from disk are removed from both the index and the cache.
pub fn run(root: &Path, index_dir: &Path, jobs: Option<usize>) -> Result<IndexStats> {
    if let Some(n) = jobs {
        rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .build_global()
            .ok(); // ignore if the global pool was already initialized
    }

    let (index, fields) = open_or_create(index_dir)?;
    let mut cache_store = cache::load(index_dir)?;

    let found = scan::find_pdfs(root);
    println!("Found {} PDF file(s), checking for changes ...", found.len());

    let pb = ProgressBar::new(found.len() as u64);
    pb.set_style(
        ProgressStyle::with_template("{bar:40.cyan/blue} {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("##-"),
    );

    // Step 1: check + extract text in parallel (only for new/changed files).
    let results: Vec<ScanResult> = found
        .par_iter()
        .progress_with(pb.clone())
        .map(|path| scan_one(path, &cache_store))
        .collect();
    pb.finish_and_clear();

    // Step 2: write to the index sequentially (tantivy parallelizes commits
    // internally; an IndexWriter must not be written to from multiple
    // threads at the same time).
    let mut writer = index.writer(64_000_000)?; // 64 MB write buffer
    let mut updated = 0usize;
    let mut failed = 0usize;
    let mut still_present: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (path, result) in found.iter().zip(results.into_iter()) {
        let key = path.to_string_lossy().to_string();
        match result {
            ScanResult::Unchanged => {
                still_present.insert(key);
            }
            ScanResult::Updated { path, pages, meta } => {
                let key = path.to_string_lossy().to_string();
                writer.delete_term(Term::from_field_text(fields.path, &key));
                let filename = path
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_default();
                for (i, page_text) in pages.iter().enumerate() {
                    let mut doc = tantivy::TantivyDocument::default();
                    doc.add_text(fields.path, &key);
                    doc.add_text(fields.filename, &filename);
                    doc.add_u64(fields.page, (i + 1) as u64);
                    doc.add_text(fields.content, page_text);
                    writer.add_document(doc)?;
                }
                cache_store.files.insert(key.clone(), meta);
                still_present.insert(key);
                updated += 1;
            }
            ScanResult::Failed { path, error } => {
                eprintln!("warning: {}: {}", path.display(), error);
                failed += 1;
            }
        }
    }

    // Step 3: purge files that disappeared from the filesystem from both
    // the index and the cache.
    let stale: Vec<String> = cache_store
        .files
        .keys()
        .filter(|k| !still_present.contains(*k))
        .cloned()
        .collect();
    for key in &stale {
        writer.delete_term(Term::from_field_text(fields.path, key));
        cache_store.files.remove(key);
    }

    writer.commit()?;
    cache::save(index_dir, &cache_store)?;

    Ok(IndexStats {
        scanned: found.len(),
        updated,
        removed: stale.len(),
        failed,
    })
}

fn scan_one(path: &Path, cache_store: &CacheStore) -> ScanResult {
    let key = path.to_string_lossy().to_string();
    let (mtime, size) = match scan::file_fingerprint(path) {
        Ok(v) => v,
        Err(e) => {
            return ScanResult::Failed {
                path: path.to_path_buf(),
                error: format!("could not read metadata: {e}"),
            }
        }
    };

    if let Some(existing) = cache_store.files.get(&key) {
        if existing.mtime_secs == mtime && existing.size == size {
            return ScanResult::Unchanged;
        }
    }

    match scan::extract_pages(path) {
        Ok(pages) => ScanResult::Updated {
            path: path.to_path_buf(),
            meta: FileMeta {
                mtime_secs: mtime,
                size,
                pages: pages.len(),
            },
            pages,
        },
        Err(e) => ScanResult::Failed {
            path: path.to_path_buf(),
            error: e.to_string(),
        },
    }
}
