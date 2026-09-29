# pdfsearch

A small, fast Rust tool that indexes a directory of PDFs (including all
subdirectories), extracts their text with `pdftotext`, and makes them
full-text searchable - both from the command line and from a local web
GUI. A hit list is the entry point to the matching PDF: the CLI opens it
with the system's default viewer, and the web GUI opens it embedded in the
browser's own PDF viewer with the search terms already highlighted.
**PDFs are only ever read, never modified.**

## Requirements

- Rust/Cargo (tested with Rust 1.75; also works with newer versions)
- `pdftotext` from Poppler
  - Debian/Ubuntu: `sudo apt install poppler-utils`
  - macOS: `brew install poppler`
- Indexing and searching work fully offline. The web GUI's highlighted PDF
  viewer prefers an internet connection in the browser (it loads pdf.js
  from a CDN) but automatically falls back to the browser's native PDF
  viewer if none is available - see "The web GUI" below.

## Build

```bash
cargo build --release
# binary ends up at target/release/pdfsearch
```

Note on `Cargo.lock`: it is deliberately pinned to dependency versions
that still compile with older Rust toolchains (roughly 1.75+), since many
current crate releases now require a noticeably newer compiler. With a
recent Rust installation, running `cargo update` is safe at any time.

## Usage

### 1. Index a directory

```bash
pdfsearch index /path/to/pdfs
```

- Recursively scans `/path/to/pdfs` for `*.pdf`.
- Extracts each file's text once with `pdftotext -layout` and splits it
  into individual pages using the page breaks `pdftotext` inserts, so
  later hits can be shown with a page number.
- Builds a full-text index (tantivy, BM25 ranking) in a cache directory
  (by default `~/.cache/pdfsearch/<name>-<hash>`, derived from the
  absolute source path; override with `--index-dir`).
- **Incremental:** running it again only checks each file's modification
  time and size. Unchanged files are skipped, new/changed files are
  re-scanned, and files that were deleted are removed from the index.
- Registers the directory so it shows up in the web GUI's overview.

```bash
pdfsearch index /path/to/pdfs --jobs 8        # limit parallelism
pdfsearch index /path/to/pdfs --index-dir ./idx
```

### 2. Search from the terminal

```bash
pdfsearch search /path/to/pdfs "termination clause"
```

Shows a relevance-ranked hit list with filename, page number and a
highlighted context snippet. A hit number then opens the matching PDF
with the operating system's default viewer.

Without a search term, an interactive mode starts for any number of
searches in a row:

```bash
pdfsearch search /path/to/pdfs
Search> contract AND termination
...
Number to open (Enter to search again): 2
Search> q
```

The query syntax comes from tantivy and supports `AND`, `OR`,
`"exact phrases"`, wildcards (`terminat*`) and field boosting (filenames
are weighted higher than body text automatically).

### 3. The web GUI

```bash
pdfsearch serve
# opens http://127.0.0.1:7878/ in your browser
```

- **Overview page:** lists every directory ever indexed, with file/page
  counts and when it was last indexed. From there you can add a new
  directory (indexed in the background), trigger a re-index, or remove a
  directory from the overview (its index files stay on disk).
- **Search page:** a search box per directory with the same relevance
  ranking and highlighted snippets as the CLI. Every preview shows several
  full sentences of context before and after the match (not just a short
  fragment), so you can judge a hit's relevance without opening the file.
  **Results are grouped by file:** if a PDF matches on several pages, it
  still shows up as a single entry (with an excerpt per matching page,
  collapsing further matches into a "+ N more pages" note) and a single
  "Open PDF" link - never one row per page.
- **Global search across all directories:** the overview page has its own
  search box ("Search everywhere") that queries every indexed directory at
  once and merges the results by relevance, showing which directory each
  hit came from. From a per-directory search you can switch to it anytime
  via "Search all directories instead".
- **Embedded PDF viewing with highlights:** each result has a single "Open
  PDF" link that opens in a **new browser tab** with the search term
  highlighted throughout the document. Browsers' native PDF viewers only
  support jumping to a page reliably - highlighting via a `#search=...`
  URL fragment is a Firefox-only (pdf.js) feature and is silently ignored
  by Chrome, Edge and Safari. To get highlighting that works consistently
  everywhere, pdfsearch embeds Mozilla's own pdf.js viewer in that tab (via
  the `pdfjs-viewer-element` web component, loaded from a CDN at
  `cdn.jsdelivr.net`) instead of relying on the browser's built-in viewer.
  **Offline fallback:** if that CDN script can't be reached (no internet on
  the machine running the *browser*) or doesn't finish loading within a few
  seconds, the page automatically falls back to the browser's native PDF
  viewer via the same `#page=&search=` URL fragment - the page jump still
  works everywhere, and the highlight still works in Firefox. The viewer
  page also has a plain "Open raw PDF file" link as a manual fallback, e.g.
  to print or save a copy - that link goes straight to the unmodified
  original file. No PDF is ever changed on disk; any highlighting only ever
  happens live, in the browser.
- `pdfsearch serve --port 8080 --no-browser` to change the port or skip
  auto-opening a browser tab.

The server only listens on `127.0.0.1` (not reachable from the network)
and only ever reads the PDF files it was asked to index - there is no way
to write to or modify them through the GUI.

## Architecture / optimizations

- **Parallel extraction:** `rayon` uses all CPU cores while indexing to
  run `pdftotext` on several files at once.
- **One process call per file:** the whole PDF text is read from stdout in
  one go (no temporary files); splitting into pages afterwards uses the
  form-feed characters `pdftotext` already inserts between pages.
- **Incremental indexing:** a JSON cache (mtime + size per file) avoids
  needlessly re-scanning unchanged PDFs.
- **Persistent, memory-mapped index:** `tantivy` (a Lucene-like full-text
  search library for Rust) keeps the index on disk via mmap, so even large
  collections stay searchable without high RAM use.
- **Zero-dependency PDF viewing:** the web GUI reuses the browser's
  already-installed PDF renderer instead of shipping one, keeping the
  binary small and avoiding a PDF-rendering dependency altogether.
- **Release profile:** LTO, a single codegen unit and `strip` are enabled
  for a small, fast binary.

## Not included

- Editing/writing PDFs (intentionally out of scope).
- OCR for scanned, textless PDFs (that would need `tesseract`/`ocrmypdf`
  upstream of this tool - `pdftotext` returns no text for those).

## Project structure

```
src/
  main.rs      CLI (clap): `index`, `search` and `serve` subcommands
  scan.rs      Recursively finding PDFs + the pdftotext call
  schema.rs    tantivy schema (path, filename, page, content)
  indexer.rs   Incrementally filling the index (parallel + cache)
  search.rs    Query execution, snippet highlighting, CLI open-by-number
  registry.rs  Registry of indexed directories, used by the web GUI
  web.rs       Local HTTP server: overview, search and PDF viewing
  model.rs     Cache data structure (Serde)
  cache.rs     Loading/saving the JSON cache
  util.rs      Deriving the index directory from the source path
```
