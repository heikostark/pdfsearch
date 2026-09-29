use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

/// Recursively searches `root` for `*.pdf` files (case-insensitive) and
/// returns the paths found. Symlink loops are avoided automatically by
/// walkdir since we don't follow symlinks.
pub fn find_pdfs(root: &Path) -> Vec<PathBuf> {
    WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| {
            p.extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.eq_ignore_ascii_case("pdf"))
                .unwrap_or(false)
        })
        .collect()
}

/// mtime (seconds since epoch) and file size - a cheap fingerprint used to
/// decide whether a file has changed, without reading its contents.
pub fn file_fingerprint(path: &Path) -> Result<(u64, u64)> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok((mtime, meta.len()))
}

/// Extracts the text of a PDF file using the external `pdftotext` tool
/// (Poppler utils) and returns the text per page as a `Vec<String>`.
/// pdftotext separates pages with a form-feed character (0x0C) by default
/// - that's exactly what we split on, so a single process invocation per
/// file is enough (no need to call pdftotext once per page, which would be
/// considerably slower).
pub fn extract_pages(path: &Path) -> Result<Vec<String>> {
    let output = Command::new("pdftotext")
        .arg("-layout")
        .arg("-enc")
        .arg("UTF-8")
        .arg(path)
        .arg("-") // write to stdout instead of a temporary file
        .output()
        .map_err(|e| anyhow!("could not start pdftotext (is poppler-utils installed?): {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "pdftotext reported an error for {}: {}",
            path.display(),
            stderr.trim()
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout).into_owned();

    let mut pages: Vec<String> = text.split('\u{0C}').map(|p| p.trim().to_string()).collect();
    // A trailing form-feed usually leaves one empty element at the end.
    if pages.last().map(|p| p.is_empty()).unwrap_or(false) {
        pages.pop();
    }
    if pages.is_empty() {
        pages.push(String::new());
    }
    Ok(pages)
}
