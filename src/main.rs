mod cache;
mod indexer;
mod model;
mod registry;
mod schema;
mod scan;
mod search;
mod util;
mod web;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// pdfsearch - indexes the PDFs in a directory tree via pdftotext and makes
/// them full-text searchable. The hit list is the entry point for opening
/// the matching PDF, either with the system's default viewer (CLI) or
/// embedded in the browser with the search terms highlighted (web GUI).
/// The PDFs themselves are never modified.
#[derive(Parser)]
#[command(name = "pdfsearch", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Recursively scan a directory and update its search index. Already
    /// indexed, unchanged files are skipped.
    Index {
        /// Root directory containing the PDFs
        path: PathBuf,
        /// Alternative location for the index (default: system cache dir)
        #[arg(long)]
        index_dir: Option<PathBuf>,
        /// Number of parallel worker threads (default: all CPU cores)
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Search the index. Without a search term, an interactive mode starts.
    Search {
        /// Root directory previously indexed with `index`
        path: PathBuf,
        /// Search term / query (tantivy syntax, e.g. "contract AND termination")
        query: Option<String>,
        /// Alternative index location (must match `index`)
        #[arg(long)]
        index_dir: Option<PathBuf>,
        /// Maximum number of hits
        #[arg(long, default_value_t = 15)]
        limit: usize,
    },
    /// Start the local web GUI: an overview of every indexed directory,
    /// full-text search, and inline PDF viewing with highlighted search
    /// terms in the browser's built-in PDF viewer.
    Serve {
        /// Port to listen on
        #[arg(long, default_value_t = 7878)]
        port: u16,
        /// Don't automatically open the browser
        #[arg(long)]
        no_browser: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Index { path, index_dir, jobs } => {
            let path = path
                .canonicalize()
                .with_context(|| format!("directory not found: {}", path.display()))?;
            let index_dir = util::resolve_index_dir(&path, index_dir)?;
            println!("Index directory: {}", index_dir.display());
            let stats = indexer::run(&path, &index_dir, jobs)?;
            registry::register(&path, &index_dir)?;
            println!(
                "Done: {} file(s) checked, {} updated, {} removed, {} failed.",
                stats.scanned, stats.updated, stats.removed, stats.failed
            );
        }
        Command::Search {
            path,
            query,
            index_dir,
            limit,
        } => {
            let path = path
                .canonicalize()
                .with_context(|| format!("directory not found: {}", path.display()))?;
            let index_dir = util::resolve_index_dir(&path, index_dir)?;
            if !index_dir.exists() {
                anyhow::bail!(
                    "No index found at {}. Run `pdfsearch index {}` first.",
                    index_dir.display(),
                    path.display()
                );
            }
            match query {
                Some(q) => search::run_once(&path, &index_dir, &q, limit)?,
                None => search::run_interactive(&path, &index_dir, limit)?,
            }
        }
        Command::Serve { port, no_browser } => {
            web::run(port, !no_browser)?;
        }
    }

    Ok(())
}
