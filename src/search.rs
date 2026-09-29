use anyhow::Result;
use std::io::{self, Write};
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::Value;
use tantivy::{ReloadPolicy, TantivyDocument};

use crate::indexer;

/// How many full sentences of context to include before/after the match
/// in a preview - the user asked for "at least 2-3 sentences before and
/// after", so we use 3 on each side.
const CONTEXT_SENTENCES_BEFORE: usize = 3;
const CONTEXT_SENTENCES_AFTER: usize = 3;
/// Hard safety cap so a page of unpunctuated/garbled PDF text (which would
/// otherwise look like one giant "sentence") can't blow up into a
/// multi-kilobyte preview.
const MAX_CONTEXT_BYTES: usize = 1400;

/// One search hit: the page it was found on, its relevance score, and a
/// context snippet. `highlights` are byte ranges into `fragment` that
/// matched a search term, so different front ends (terminal, HTML) can
/// render the emphasis however suits them.
pub struct Hit {
    pub path: String,
    pub filename: String,
    pub page: u64,
    pub score: f32,
    pub fragment: String,
    pub highlights: Vec<(usize, usize)>,
}

/// One matching page within a `FileHit`, i.e. a `Hit` stripped of the
/// file-level fields once it has been grouped.
pub struct PageMatch {
    pub page: u64,
    pub score: f32,
    pub fragment: String,
    pub highlights: Vec<(usize, usize)>,
}

/// All matches inside a single PDF, merged into one result so a document
/// that matches on several pages shows up once - with one link to open it
/// - instead of once per matching page. `pages` is sorted by relevance,
/// best match first.
pub struct FileHit {
    pub path: String,
    pub filename: String,
    pub best_score: f32,
    pub pages: Vec<PageMatch>,
}

/// Groups page-level hits by file. Used directly by the CLI; the web GUI
/// uses [`group_by_file_with_tag`] instead so it can carry each hit's
/// source directory along through the grouping (needed for the
/// cross-directory global search).
pub fn group_by_file(hits: Vec<Hit>) -> Vec<FileHit> {
    group_by_file_with_tag(hits.into_iter().map(|h| ((), h)).collect())
        .into_iter()
        .map(|(_, g)| g)
        .collect()
}

/// Same as [`group_by_file`], but each hit carries an extra tag (e.g. the
/// root directory it came from) that is preserved on the resulting group -
/// all pages of one file always share the same tag, so the first one seen
/// is kept.
pub fn group_by_file_with_tag<T: Clone>(items: Vec<(T, Hit)>) -> Vec<(T, FileHit)> {
    use std::collections::HashMap;
    let mut map: HashMap<String, (T, FileHit)> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for (tag, h) in items {
        let path = h.path.clone();
        if !map.contains_key(&path) {
            order.push(path.clone());
            map.insert(
                path.clone(),
                (
                    tag,
                    FileHit {
                        path: h.path.clone(),
                        filename: h.filename.clone(),
                        best_score: f32::MIN,
                        pages: Vec::new(),
                    },
                ),
            );
        }
        let entry = map.get_mut(&path).unwrap();
        entry.1.best_score = entry.1.best_score.max(h.score);
        entry.1.pages.push(PageMatch {
            page: h.page,
            score: h.score,
            fragment: h.fragment,
            highlights: h.highlights,
        });
    }
    let mut groups: Vec<(T, FileHit)> = order.into_iter().filter_map(|p| map.remove(&p)).collect();
    for (_, g) in &mut groups {
        g.pages
            .sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    }
    groups.sort_by(|a, b| b.1.best_score.partial_cmp(&a.1.best_score).unwrap_or(std::cmp::Ordering::Equal));
    groups
}

/// Runs a query against the index stored in `index_dir` and returns the
/// hits ordered by relevance. This is the shared search backend used by
/// the CLI, the per-directory web search and the cross-directory global
/// search.
pub fn query(index_dir: &Path, query_str: &str, limit: usize) -> Result<Vec<Hit>> {
    let (index, fields) = indexer::open_or_create(index_dir)?;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::OnCommitWithDelay)
        .try_into()?;
    let searcher: tantivy::Searcher = reader.searcher();

    let mut query_parser = QueryParser::for_index(&index, vec![fields.content, fields.filename]);
    query_parser.set_field_boost(fields.filename, 1.5);
    let parsed_query = query_parser.parse_query(query_str)?;

    let top_docs = searcher.search(&parsed_query, &TopDocs::with_limit(limit))?;
    let terms = extract_terms(query_str);

    let mut hits = Vec::with_capacity(top_docs.len());
    for (score, addr) in top_docs {
        let doc: TantivyDocument = searcher.doc(addr)?;
        let path = doc
            .get_first(fields.path)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let filename = doc
            .get_first(fields.filename)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let page = doc.get_first(fields.page).and_then(|v| v.as_u64()).unwrap_or(0);
        let content = doc
            .get_first(fields.content)
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let (fragment, highlights) = build_snippet(content, &terms);

        hits.push(Hit {
            path,
            filename,
            page,
            score,
            fragment,
            highlights,
        });
    }
    Ok(hits)
}

/// Breaks a (possibly boolean/quoted) query string into plain lowercase
/// search words used for locating and highlighting matches in the full
/// page text. This intentionally doesn't try to replicate tantivy's
/// tokenizer/stemming - a simple case-insensitive substring match is
/// enough to build a good preview, and ranking still comes from tantivy
/// itself, so the two never disagree about *whether* a page matched.
fn extract_terms(query_str: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    query_str
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() >= 2 && !matches!(w.as_str(), "and" | "or" | "not"))
        .filter(|w| seen.insert(w.clone()))
        .collect()
}

/// Builds a generous, sentence-aware preview around the first match of any
/// search term in `text`: several full sentences before and after the
/// hit, instead of a short fixed-length snippet. Falls back to the start
/// of the text if none of the terms can be found verbatim (e.g. because
/// the match came from the filename rather than the page content).
fn build_snippet(text: &str, terms: &[String]) -> (String, Vec<(usize, usize)>) {
    if text.trim().is_empty() {
        return (String::new(), Vec::new());
    }
    let lower = text.to_lowercase();
    let anchor = terms.iter().filter_map(|t| lower.find(t.as_str())).min().unwrap_or(0);

    let bounds = sentence_starts(text);
    let sentence_idx = bounds.iter().rposition(|&b| b <= anchor).unwrap_or(0);

    let start_idx = sentence_idx.saturating_sub(CONTEXT_SENTENCES_BEFORE);
    let end_idx = (sentence_idx + CONTEXT_SENTENCES_AFTER).min(bounds.len().saturating_sub(1));

    let mut start = bounds[start_idx];
    let mut end = if end_idx + 1 < bounds.len() {
        bounds[end_idx + 1]
    } else {
        text.len()
    };

    if end - start > MAX_CONTEXT_BYTES {
        let mid = anchor.clamp(start, end);
        let half = MAX_CONTEXT_BYTES / 2;
        start = floor_boundary(text, mid.saturating_sub(half).max(start));
        end = ceil_boundary(text, (mid + half).min(end));
    }

    let fragment = text[start..end].trim().to_string();
    let highlights = find_highlights(&fragment, terms);
    (fragment, highlights)
}

/// Byte offsets where a new sentence starts, based on `.`, `!` and `?`
/// followed by whitespace (a good-enough heuristic for extracted PDF
/// text; it isn't abbreviation-aware, but that only ever makes the
/// preview include one extra short sentence, it never breaks anything).
fn sentence_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    let mut chars = text.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if matches!(ch, '.' | '!' | '?') {
            let mut end = idx + ch.len_utf8();
            while let Some(&(_, c2)) = chars.peek() {
                if matches!(c2, '"' | '\'' | ')' | ']') {
                    end += c2.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let next_is_boundary = text[end..].chars().next().map(|c| c.is_whitespace()).unwrap_or(true);
            if next_is_boundary {
                let mut k = end;
                while let Some(c2) = text[k..].chars().next() {
                    if c2.is_whitespace() {
                        k += c2.len_utf8();
                    } else {
                        break;
                    }
                }
                if k < text.len() && k > *starts.last().unwrap() {
                    starts.push(k);
                }
            }
        }
    }
    starts
}

fn floor_boundary(text: &str, mut idx: usize) -> usize {
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_boundary(text: &str, mut idx: usize) -> usize {
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Finds every (merged, non-overlapping) occurrence of any search term in
/// `fragment`, case-insensitively. Ranges that don't land on a valid UTF-8
/// char boundary (possible in rare cases where lower-casing changes a
/// character's byte length) are dropped rather than risking a panic later
/// when the fragment is sliced for rendering.
fn find_highlights(fragment: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let lower = fragment.to_lowercase();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for term in terms {
        let mut start = 0usize;
        while start <= lower.len() {
            match lower[start..].find(term.as_str()) {
                Some(pos) => {
                    let s = start + pos;
                    let e = s + term.len();
                    ranges.push((s, e));
                    start = s + term.len().max(1);
                }
                None => break,
            }
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in ranges {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }
    merged.retain(|&(s, e)| s < e && e <= fragment.len() && fragment.is_char_boundary(s) && fragment.is_char_boundary(e));
    merged
}

/// Renders a fragment as plain text with the matched ranges shown in bold
/// via ANSI escape codes, for terminal output.
fn ansi_highlight_fragment(fragment: &str, highlights: &[(usize, usize)]) -> String {
    let mut out = String::new();
    let mut last = 0usize;
    for (start, end) in highlights {
        out.push_str(&fragment[last..*start]);
        out.push_str("\x1b[1m");
        out.push_str(&fragment[*start..*end]);
        out.push_str("\x1b[0m");
        last = *end;
    }
    out.push_str(&fragment[last..]);
    out
}

/// How many of a file's matching pages to print excerpts for in the
/// terminal before collapsing the rest into a "+ N more pages" note.
const CLI_EXCERPTS_PER_FILE: usize = 2;

fn print_groups(groups: &[FileHit]) {
    if groups.is_empty() {
        println!("No matches.");
        return;
    }
    for (i, g) in groups.iter().enumerate() {
        println!(
            "[{:>2}] {}  ({} matching page(s), best relevance {:.2})",
            i + 1,
            g.filename,
            g.pages.len(),
            g.best_score
        );
        println!("     {}", g.path);
        for p in g.pages.iter().take(CLI_EXCERPTS_PER_FILE) {
            let text = ansi_highlight_fragment(&p.fragment, &p.highlights).replace('\n', " ");
            println!("       page {}: ... {} ...", p.page, text);
        }
        if g.pages.len() > CLI_EXCERPTS_PER_FILE {
            let more: Vec<String> = g.pages[CLI_EXCERPTS_PER_FILE..]
                .iter()
                .map(|p| p.page.to_string())
                .collect();
            println!(
                "       + {} more matching page(s): {}",
                g.pages.len() - CLI_EXCERPTS_PER_FILE,
                more.join(", ")
            );
        }
        println!();
    }
}

fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn open_group(groups: &[FileHit], input: &str) {
    if let Ok(n) = input.parse::<usize>() {
        if n >= 1 && n <= groups.len() {
            let g = &groups[n - 1];
            println!(
                "Opening {} ({} matching page(s)) ...",
                g.filename,
                g.pages.len()
            );
            if let Err(e) = open::that(&g.path) {
                eprintln!("Could not open file: {e}");
            }
            return;
        }
    }
    println!("Invalid selection.");
}

/// One-shot search (non-interactive), e.g. for scripting. Prints the hit
/// list grouped by file; afterwards a number can optionally be chosen to
/// open the corresponding PDF.
pub fn run_once(root: &Path, index_dir: &Path, query_str: &str, limit: usize) -> Result<()> {
    let hits = query(index_dir, query_str, limit)?;
    let groups = group_by_file(hits);
    println!("Searching in: {}\n", root.display());
    print_groups(&groups);
    if !groups.is_empty() {
        let choice = prompt("Number to open (Enter to quit): ")?;
        if !choice.is_empty() {
            open_group(&groups, &choice);
        }
    }
    Ok(())
}

/// Interactive search loop: search as many times as you like, open a
/// result (one whole PDF, even if several of its pages matched) by
/// number, "q" / an empty line exits.
pub fn run_interactive(root: &Path, index_dir: &Path, limit: usize) -> Result<()> {
    println!("Searching in: {}", root.display());
    println!("Enter a search term (empty / 'q' to quit).\n");

    loop {
        let query_str = prompt("Search> ")?;
        if query_str.is_empty() || query_str.eq_ignore_ascii_case("q") {
            break;
        }
        let hits = match query(index_dir, &query_str, limit) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("Invalid query: {e}");
                continue;
            }
        };
        let groups = group_by_file(hits);
        print_groups(&groups);
        if groups.is_empty() {
            continue;
        }
        let choice = prompt("Number to open (Enter to search again): ")?;
        if !choice.is_empty() {
            open_group(&groups, &choice);
        }
        println!();
    }
    Ok(())
}
