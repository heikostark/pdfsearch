use anyhow::{Context, Result};
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use std::collections::HashMap;
use std::fs::File;
use std::io::Cursor;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime};
use tiny_http::{Header, Method, Response, Server};

use crate::cache;
use crate::indexer;
use crate::registry::{self, RegisteredRoot};
use crate::search::{self, FileHit};
use crate::util;

type HtmlResponse = Response<Cursor<Vec<u8>>>;

/// Starts the local web GUI and blocks serving requests until the process
/// is terminated (e.g. with Ctrl+C).
pub fn run(port: u16, open_browser: bool) -> Result<()> {
    let server = Server::http(("127.0.0.1", port))
        .map_err(|e| anyhow::anyhow!("could not start the web server on port {port}: {e}"))?;
    let url = format!("http://127.0.0.1:{port}/");
    println!("pdfsearch web GUI running at {url}");
    println!("Press Ctrl+C to stop.");
    if open_browser {
        let _ = open::that(&url);
    }

    for mut request in server.incoming_requests() {
        let method = request.method().clone();
        let raw_url = request.url().to_string();
        let (path, query) = split_query(&raw_url);
        let params = parse_query(&query);

        // Serving the PDF bytes is handled separately: it streams the file
        // directly and uses a different Response<R> type than the plain
        // HTML/text responses below.
        if method == Method::Get && path == "/view" {
            if let Err(e) = serve_pdf(request, &params) {
                eprintln!("error serving PDF: {e}");
            }
            continue;
        }

        let mut body = String::new();
        if method == Method::Post {
            let _ = request.as_reader().read_to_string(&mut body);
        }
        let form = parse_query(&body);

        let response: HtmlResponse = match (method, path.as_str()) {
            (Method::Get, "/") => html_response(render_overview()),
            (Method::Post, "/add") => match handle_add(&form) {
                Ok(()) => redirect("/"),
                Err(e) => html_response(error_page(&e.to_string())),
            },
            (Method::Get, "/reindex") => match handle_reindex(&params) {
                Ok(()) => redirect("/"),
                Err(e) => html_response(error_page(&e.to_string())),
            },
            (Method::Post, "/remove") => match handle_remove(&form) {
                Ok(()) => redirect("/"),
                Err(e) => html_response(error_page(&e.to_string())),
            },
            (Method::Get, "/search") => match render_search(&params) {
                Ok(body) => html_response(body),
                Err(e) => html_response(error_page(&e.to_string())),
            },
            (Method::Get, "/pdf-viewer") => html_response(render_pdf_viewer(&params)),
            _ => not_found(),
        };

        if let Err(e) = request.respond(response) {
            eprintln!("failed to send response: {e}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Tiny request helpers (no web framework - keeps the dependency tree
// small and avoids yet another round of MSRV chasing).
// ---------------------------------------------------------------------

fn split_query(url: &str) -> (String, String) {
    match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.to_string(), String::new()),
    }
}

fn parse_query(qs: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in qs.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let decode = |s: &str| {
            percent_decode_str(&s.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        };
        map.insert(decode(k), decode(v));
    }
    map
}

fn encode(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn html_response(body: String) -> HtmlResponse {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    Response::from_string(body).with_header(header)
}

fn redirect(location: &str) -> HtmlResponse {
    let header = Header::from_bytes(&b"Location"[..], location.as_bytes()).unwrap();
    Response::from_string("").with_status_code(303).with_header(header)
}

fn not_found() -> HtmlResponse {
    html_response(page("Not found", "<p>Not found.</p>".to_string()))
}

fn error_page(message: &str) -> String {
    page(
        "Error",
        format!(
            "<p class=\"error\">{}</p><p><a href=\"/\">Back to overview</a></p>",
            html_escape(message)
        ),
    )
}

// ---------------------------------------------------------------------
// Serving the raw PDF (browser-native, inline viewer)
// ---------------------------------------------------------------------

/// Streams a PDF file back to the browser with `Content-Type: application/pdf`
/// and `Content-Disposition: inline`, so it opens directly in the browser's
/// built-in PDF viewer instead of downloading. The page and search term are
/// passed by the caller as a URL *fragment* (`#page=N&search=...`), which
/// browsers never send to the server but do hand to their PDF viewer, so
/// the requested page is shown and the search term is pre-highlighted -
/// all without pdfsearch having to render PDFs itself.
fn serve_pdf(request: tiny_http::Request, params: &HashMap<String, String>) -> Result<()> {
    let root = params.get("root").cloned().unwrap_or_default();
    let file = params.get("file").cloned().unwrap_or_default();

    let root_canon = PathBuf::from(&root)
        .canonicalize()
        .context("unknown root directory")?;
    let file_canon = PathBuf::from(&file)
        .canonicalize()
        .context("unknown file")?;

    if !file_canon.starts_with(&root_canon) {
        let resp = Response::from_string("Forbidden: file is outside the requested root").with_status_code(403);
        request.respond(resp)?;
        return Ok(());
    }

    let f = File::open(&file_canon).with_context(|| format!("could not open {}", file_canon.display()))?;
    let filename = file_canon
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("document.pdf");

    let content_type = Header::from_bytes(&b"Content-Type"[..], &b"application/pdf"[..]).unwrap();
    let disposition = Header::from_bytes(
        &b"Content-Disposition"[..],
        format!("inline; filename=\"{filename}\"").as_bytes(),
    )
    .unwrap();

    let response = Response::from_file(f).with_header(content_type).with_header(disposition);
    request.respond(response)?;
    Ok(())
}

// ---------------------------------------------------------------------
// Route handlers that mutate state (add / reindex / remove a directory)
// ---------------------------------------------------------------------

fn handle_add(form: &HashMap<String, String>) -> Result<()> {
    let raw = form.get("path").cloned().unwrap_or_default();
    if raw.trim().is_empty() {
        anyhow::bail!("please enter a directory path");
    }
    let path = PathBuf::from(raw.trim());
    if !path.is_dir() {
        anyhow::bail!("'{}' is not an existing directory", path.display());
    }
    let path = path
        .canonicalize()
        .with_context(|| format!("directory not found: {}", path.display()))?;
    let index_dir = util::resolve_index_dir(&path, None)?;
    registry::register(&path, &index_dir)?;
    spawn_index(path, index_dir);
    Ok(())
}

fn handle_reindex(params: &HashMap<String, String>) -> Result<()> {
    let root = params.get("root").cloned().unwrap_or_default();
    let root_path = PathBuf::from(&root).canonicalize().context("unknown directory")?;
    let entry = registry::list()?
        .into_iter()
        .find(|e| e.root == root_path)
        .context("this directory is not registered")?;
    spawn_index(entry.root, entry.index_dir);
    Ok(())
}

fn handle_remove(form: &HashMap<String, String>) -> Result<()> {
    let root = form.get("root").cloned().unwrap_or_default();
    registry::unregister(&PathBuf::from(root))?;
    Ok(())
}

fn spawn_index(root: PathBuf, index_dir: PathBuf) {
    thread::spawn(move || {
        println!("Indexing {} ...", root.display());
        match indexer::run(&root, &index_dir, None) {
            Ok(stats) => println!(
                "Finished indexing {}: {} scanned, {} updated, {} removed, {} failed.",
                root.display(),
                stats.scanned,
                stats.updated,
                stats.removed,
                stats.failed
            ),
            Err(e) => eprintln!("Indexing {} failed: {e}", root.display()),
        }
    });
}

// ---------------------------------------------------------------------
// Page rendering
// ---------------------------------------------------------------------

fn page(title: &str, body: String) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - pdfsearch</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
          max-width: 860px; margin: 2rem auto; padding: 0 1rem; line-height: 1.5; }}
  h1 {{ font-size: 1.4rem; }}
  h1 a {{ text-decoration: none; color: inherit; }}
  table {{ width: 100%; border-collapse: collapse; margin: 1rem 0; }}
  th, td {{ text-align: left; padding: 0.5rem; border-bottom: 1px solid #ddd; vertical-align: top; }}
  form.inline {{ display: inline; }}
  input[type=text] {{ padding: 0.4rem; width: 100%; box-sizing: border-box; }}
  button, input[type=submit] {{ padding: 0.4rem 0.8rem; cursor: pointer; }}
  .muted {{ color: #777; font-size: 0.9rem; }}
  .hit {{ margin: 1rem 0; padding-bottom: 1rem; border-bottom: 1px solid #eee; }}
  .hit .meta {{ font-size: 0.85rem; color: #777; }}
  .excerpt {{ margin: 0.4rem 0 0.4rem 1rem; }}
  mark {{ background: #ffe066; padding: 0 2px; }}
  .error {{ color: #b00020; }}
  .actions form, .actions a {{ margin-right: 0.5rem; }}
  code {{ background: #f2f2f2; padding: 1px 4px; border-radius: 3px; }}
</style>
</head>
<body>
<h1><a href="/">pdfsearch</a></h1>
{body}
</body>
</html>"#
    )
}

fn humanize_age(t: Option<SystemTime>) -> String {
    let Some(t) = t else {
        return "never".to_string();
    };
    let secs = SystemTime::now().duration_since(t).unwrap_or(Duration::ZERO).as_secs();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{} h ago", secs / 3600)
    } else {
        format!("{} d ago", secs / 86_400)
    }
}

struct RootStats {
    root: PathBuf,
    file_count: usize,
    page_count: usize,
    last_indexed: Option<SystemTime>,
}

fn root_stats(entry: &RegisteredRoot) -> RootStats {
    let store = cache::load(&entry.index_dir).unwrap_or_default();
    let file_count = store.files.len();
    let page_count = store.files.values().map(|f| f.pages).sum();
    let last_indexed = std::fs::metadata(entry.index_dir.join("cache.json"))
        .ok()
        .and_then(|m| m.modified().ok());
    RootStats {
        root: entry.root.clone(),
        file_count,
        page_count,
        last_indexed,
    }
}

fn render_overview() -> String {
    let roots = registry::list().unwrap_or_default();

    let rows = if roots.is_empty() {
        "<tr><td colspan=\"4\" class=\"muted\">No directories indexed yet - add one below.</td></tr>".to_string()
    } else {
        roots
            .iter()
            .map(root_stats)
            .map(|s| {
                let root_str = s.root.to_string_lossy().to_string();
                let root_enc = encode(&root_str);
                format!(
                    r#"<tr>
  <td><code>{root_html}</code></td>
  <td>{files} file(s)<br><span class="muted">{pages} page(s)</span></td>
  <td class="muted">{age}</td>
  <td class="actions">
    <a href="/search?root={root_enc}">Search</a>
    <a href="/reindex?root={root_enc}">Reindex</a>
    <form class="inline" method="post" action="/remove" onsubmit="return confirm('Remove this directory from the overview? (Index files on disk are kept.)');">
      <input type="hidden" name="root" value="{root_attr}">
      <button type="submit">Remove</button>
    </form>
  </td>
</tr>"#,
                    root_html = html_escape(&root_str),
                    files = s.file_count,
                    pages = s.page_count,
                    age = humanize_age(s.last_indexed),
                    root_enc = root_enc,
                    root_attr = html_escape(&root_str),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let body = format!(
        r#"<p class="muted">Overview of every directory scanned with pdfsearch. Click "Search" to look
for text inside one directory's PDFs, or "Reindex" to pick up new/changed files.</p>

<form method="get" action="/search">
  <p><input type="text" name="q" placeholder="Search across ALL indexed directories ..."></p>
  <p><button type="submit">Search everywhere</button></p>
</form>

<table>
<thead><tr><th>Directory</th><th>Contents</th><th>Last indexed</th><th>Actions</th></tr></thead>
<tbody>
{rows}
</tbody>
</table>

<h2>Add a directory</h2>
<form method="post" action="/add">
  <p><input type="text" name="path" placeholder="/path/to/pdfs" required></p>
  <p><button type="submit">Add and index</button></p>
</form>
<p class="muted">Indexing runs in the background; reload this page after a moment to see updated counts.</p>"#
    );

    page("Overview", body)
}

fn render_search(params: &HashMap<String, String>) -> Result<String> {
    let root = params.get("root").cloned().unwrap_or_default();
    let q = params.get("q").cloned().unwrap_or_default();
    let is_global = root.trim().is_empty();

    let results_html = if q.trim().is_empty() {
        String::new()
    } else if is_global {
        render_global_hits(&q)?
    } else {
        let root_path = PathBuf::from(&root);
        let index_dir = util::resolve_index_dir(&root_path, None)?;
        if !index_dir.exists() {
            "<p class=\"error\">No index found for this directory yet. Go back to the <a href=\"/\">overview</a> and reindex it first.</p>".to_string()
        } else {
            // A generous raw-hit limit here: several pages of the same
            // file collapse into one result below, so we want enough raw
            // page hits to still end up with a good number of distinct
            // files after grouping.
            let hits = search::query(&index_dir, &q, 60)?;
            let tagged: Vec<(String, search::Hit)> = hits.into_iter().map(|h| (root.clone(), h)).collect();
            let groups = search::group_by_file_with_tag(tagged);
            render_groups(&groups, &q, false)
        }
    };

    let hidden_root = if is_global {
        String::new()
    } else {
        format!("<input type=\"hidden\" name=\"root\" value=\"{}\">", html_escape(&root))
    };

    let scope_note = if is_global {
        "Searching across <strong>all indexed directories</strong>.".to_string()
    } else {
        format!(
            "Searching in <code>{}</code>. <a href=\"/search?q={}\">Search all directories instead</a>",
            html_escape(&root),
            encode(&q)
        )
    };

    let body = format!(
        r#"<p><a href="/">&larr; Overview</a></p>
<form method="get" action="/search">
  {hidden_root}
  <p><input type="text" name="q" value="{q_val}" placeholder="Search text ..." autofocus></p>
  <p><button type="submit">Search</button></p>
</form>
<p class="muted">{scope_note}</p>
{results_html}"#,
        hidden_root = hidden_root,
        q_val = html_escape(&q),
        scope_note = scope_note,
        results_html = results_html,
    );

    Ok(page("Search", body))
}

/// Runs the query against every registered directory's index and merges
/// the results into a single relevance-ranked list, grouped by file, so
/// the user doesn't have to know which directory a document lives in to
/// find it - and a document that matches in several directories' copies,
/// or on several of its own pages, still only shows up once per copy.
fn render_global_hits(query_str: &str) -> Result<String> {
    let roots = registry::list().unwrap_or_default();
    if roots.is_empty() {
        return Ok("<p class=\"muted\">No directories indexed yet - add one from the overview page.</p>".to_string());
    }

    const PER_ROOT_LIMIT: usize = 60;
    const TOTAL_FILE_LIMIT: usize = 20;

    let mut combined: Vec<(String, search::Hit)> = Vec::new();
    for entry in &roots {
        if !entry.index_dir.exists() {
            continue;
        }
        match search::query(&entry.index_dir, query_str, PER_ROOT_LIMIT) {
            Ok(hits) => {
                let root_str = entry.root.to_string_lossy().to_string();
                combined.extend(hits.into_iter().map(|h| (root_str.clone(), h)));
            }
            Err(e) => eprintln!("search failed for {}: {e}", entry.root.display()),
        }
    }

    let mut groups = search::group_by_file_with_tag(combined);
    groups.truncate(TOTAL_FILE_LIMIT);

    Ok(render_groups(&groups, query_str, true))
}

/// Renders a fragment as HTML, wrapping the matched ranges in `<mark>` so
/// the browser highlights them - mirrors the ANSI highlighting used for
/// the terminal in `search.rs`.
fn html_highlight_fragment(fragment: &str, highlights: &[(usize, usize)]) -> String {
    let mut out = String::new();
    let mut last = 0usize;
    for (start, end) in highlights {
        out.push_str(&html_escape(&fragment[last..*start]));
        out.push_str("<mark>");
        out.push_str(&html_escape(&fragment[*start..*end]));
        out.push_str("</mark>");
        last = *end;
    }
    out.push_str(&html_escape(&fragment[last..]));
    out
}

/// How many of a file's matching pages get their own excerpt in the
/// results list before the rest are collapsed into a "+ N more pages"
/// note. Regardless of how many pages are shown here, the "Open PDF" link
/// always opens the *whole* document once, and the browser's own PDF
/// viewer highlights every occurrence of the search term throughout the
/// document - not just the pages listed below.
const WEB_EXCERPTS_PER_FILE: usize = 3;

/// Renders the page that actually shows a PDF to the user, with the
/// search term highlighted. Browsers' own built-in PDF viewers only
/// support jumping to a page reliably; searching/highlighting via a URL
/// fragment (`#search=...`) is a Firefox-only (pdf.js) feature and is
/// silently ignored by Chrome, Edge and Safari's PDFium/WebKit viewers.
/// To get consistent highlighting everywhere, this page instead embeds
/// Mozilla's own pdf.js viewer via the `pdfjs-viewer-element` web
/// component (loaded from a CDN), which reliably supports both `page` and
/// `search`/`phrase` attributes across all browsers. The raw file is still
/// served read-only from `/view`; this page never modifies it.
/// Renders the page that actually shows a PDF to the user, with the
/// search term highlighted. Browsers' own built-in PDF viewers only
/// support jumping to a page reliably; searching/highlighting via a URL
/// fragment (`#search=...`) is a Firefox-only (pdf.js) feature and is
/// silently ignored by Chrome, Edge and Safari's PDFium/WebKit viewers.
/// To get highlighting that works consistently everywhere, this page
/// embeds Mozilla's own pdf.js viewer via the `pdfjs-viewer-element` web
/// component, loaded from a CDN. If that can't be reached (no internet
/// connection on the *browser's* machine), a small inline script falls
/// back to the browser's native PDF viewer using the very same
/// `#page=&search=` URL fragment - page jumps then still work everywhere,
/// and highlighting still works in Firefox. Either way the raw file is
/// served read-only from `/view` and is never modified.
fn render_pdf_viewer(params: &HashMap<String, String>) -> String {
    let root = params.get("root").cloned().unwrap_or_default();
    let file = params.get("file").cloned().unwrap_or_default();
    let page = params.get("page").cloned().unwrap_or_default();
    let search = params.get("search").cloned().unwrap_or_default();

    let pdf_src = format!("/view?root={}&file={}", encode(&root), encode(&file));
    let pdf_src_attr = html_escape(&pdf_src);

    let filename = PathBuf::from(&file)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| "document.pdf".to_string());

    let page_attr = if page.trim().is_empty() {
        String::new()
    } else {
        format!(" page=\"{}\"", html_escape(&page))
    };
    let search_attr = if search.trim().is_empty() {
        String::new()
    } else {
        format!(" search=\"{}\" phrase=\"false\"", html_escape(&search))
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - pdfsearch</title>
<script>
  // Fallback for when the pdf.js viewer can't be loaded from the CDN
  // below (e.g. no internet access): jump straight to the browser's own
  // PDF viewer via the #page=&search= URL fragment instead. That still
  // jumps to the right page in every browser, and still highlights the
  // search term in Firefox (Chrome/Edge/Safari simply ignore that part
  // of the fragment and just show the page).
  var PDFSEARCH_SRC = {pdf_src_js};
  var PDFSEARCH_PAGE = {page_js};
  var PDFSEARCH_SEARCH = {search_js};
  var pdfsearchFellBack = false;
  function pdfsearchFallback() {{
    if (pdfsearchFellBack) return;
    pdfsearchFellBack = true;
    var frag = [];
    if (PDFSEARCH_PAGE) frag.push('page=' + encodeURIComponent(PDFSEARCH_PAGE));
    if (PDFSEARCH_SEARCH) frag.push('search=' + encodeURIComponent(PDFSEARCH_SEARCH));
    window.location.replace(PDFSEARCH_SRC + (frag.length ? '#' + frag.join('&') : ''));
  }}
  var pdfsearchFallbackTimer = setTimeout(pdfsearchFallback, 4000);
</script>
<script type="module"
        src="https://cdn.jsdelivr.net/npm/pdfjs-viewer-element@3/dist/pdfjs-viewer-element.js"
        onerror="pdfsearchFallback()"
        onload="customElements.whenDefined('pdfjs-viewer-element').then(function() {{ clearTimeout(pdfsearchFallbackTimer); }})"></script>
<style>
  html, body {{ margin: 0; height: 100%; }}
  .topbar {{ display: flex; align-items: center; gap: 1rem; padding: 0.5rem 1rem;
             font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
             font-size: 0.9rem; background: #222; color: #eee; box-sizing: border-box; }}
  .topbar span {{ overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
  .topbar a {{ color: #9cf; white-space: nowrap; }}
  pdfjs-viewer-element {{ display: block; width: 100%; height: calc(100% - 2.5rem); }}
</style>
</head>
<body>
<div class="topbar">
  <span>{title}</span>
  <a href="{pdf_src_attr}" target="_blank" rel="noopener">Open raw PDF file in a new tab &#8599;</a>
</div>
<pdfjs-viewer-element src="{pdf_src_attr}"{page_attr}{search_attr}></pdfjs-viewer-element>
</body>
</html>"#,
        title = html_escape(&filename),
        pdf_src_attr = pdf_src_attr,
        page_attr = page_attr,
        search_attr = search_attr,
        pdf_src_js = js_string_literal(&pdf_src),
        page_js = js_string_literal(&page),
        search_js = js_string_literal(&search),
    )
}

/// Encodes a string as a JSON/JS string literal so it can be embedded
/// directly inside a `<script>` block, including any unusual characters a
/// file or directory name might contain. A defensive `</` -> `<\/` pass
/// guards against a pathological name accidentally closing the
/// surrounding `<script>` tag.
fn js_string_literal(s: &str) -> String {
    let json = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string());
    json.replace("</", "<\\/")
}

fn render_groups(groups: &[(String, FileHit)], query_str: &str, show_root: bool) -> String {
    if groups.is_empty() {
        return "<p>No matches.</p>".to_string();
    }
    let q_enc = encode(query_str);

    groups
        .iter()
        .map(|(root, g)| {
            let root_enc = encode(root);
            let file_enc = encode(&g.path);
            // Land on the best-matching page; the browser's built-in PDF
            // search highlights every match in the whole document anyway,
            // so this is just a sensible starting point, not a limit on
            // which highlights are shown.
            let landing_page = g.pages.first().map(|p| p.page).unwrap_or(1);
            let view_href = format!("/pdf-viewer?root={root_enc}&file={file_enc}&page={landing_page}&search={q_enc}");

            let excerpts = g
                .pages
                .iter()
                .take(WEB_EXCERPTS_PER_FILE)
                .map(|p| {
                    format!(
                        r#"<div class="excerpt"><span class="meta">page {page}</span> &hellip; {snippet} &hellip;</div>"#,
                        page = p.page,
                        snippet = html_highlight_fragment(&p.fragment, &p.highlights),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");

            let more = if g.pages.len() > WEB_EXCERPTS_PER_FILE {
                let more_pages: Vec<String> = g.pages[WEB_EXCERPTS_PER_FILE..]
                    .iter()
                    .map(|p| p.page.to_string())
                    .collect();
                format!(
                    "<div class=\"meta\">+ {} more matching page(s): {}</div>",
                    g.pages.len() - WEB_EXCERPTS_PER_FILE,
                    more_pages.join(", ")
                )
            } else {
                String::new()
            };

            let root_line = if show_root {
                format!("<div class=\"meta\">in <code>{}</code></div>", html_escape(root))
            } else {
                String::new()
            };

            let pages_word = if g.pages.len() == 1 { "page" } else { "pages" };

            format!(
                r#"<div class="hit">
  <div><strong>{filename}</strong> <span class="meta">{n} matching {pages_word}, best relevance {score:.2}</span></div>
  <div class="meta">{path}</div>
  {root_line}
  {excerpts}
  {more}
  <div><a href="{view_href}" target="_blank" rel="noopener">Open PDF with all highlights &#8599;</a></div>
</div>"#,
                filename = html_escape(&g.filename),
                n = g.pages.len(),
                pages_word = pages_word,
                score = g.best_score,
                path = html_escape(&g.path),
                root_line = root_line,
                excerpts = excerpts,
                more = more,
                view_href = view_href,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

