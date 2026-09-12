//! chisel — a simple static site generator
//! Rust port of mc.py · 2025
//!
//! Key design points
//! ─────────────────
//! • `comrak`    replaces Python-Markdown: GFM tables, footnotes,
//!               strikethrough, smart punctuation (smartypants), fenced code.
//! • `minijinja` replaces Jinja2: same template syntax, same filter names.
//! • `{% markdown %}…{% endmarkdown %}` blocks (from j2m.py) are rewritten
//!   in-process to `{% set _mdN %}…{% endset %}{{ _mdN | markdown }}` so
//!   Jinja expressions inside the block are expanded first, then the result
//!   is rendered as Markdown — identical semantics to the Python extension.
//! • `rayon`     parallelises the notes-folder walk: each .md file is parsed
//!   on a separate thread; template rendering stays sequential (one env, no
//!   cross-thread borrow needed).
//! • Rust's borrow checker enforces that `Entry` values produced in parallel
//!   are fully owned before they reach the single-threaded rendering phase.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use chrono::{Datelike, Local, NaiveDateTime};
use comrak::{markdown_to_html, Options};
use minijinja::{context, Environment, Error, ErrorKind};
use rayon::prelude::*;
use regex::Regex;
use walkdir::WalkDir;

// ── Configuration ─────────────────────────────────────────────────────────────
//   Mirror of config.py — edit here to change paths.

/// Location of markdown post files (relative to $HOME)
const POSTS: &str = "Sites/notes/";
/// Local www output folder (relative to $HOME)
const WWW: &str = "Sites/home.lo/";
/// Jinja2 template folder (relative to $HOME)
const TMPL: &str = "Sites/templates/";
/// Number of recent posts included in the JSON feed
const SHOW: usize = 3;

/// Input date format used in the second line of every note
const FMT_IN: &str = "%Y-%m-%d %H:%M";
/// ISO-8601 date for the JSON feed
const FMT_FEED: &str = "%Y-%m-%dT%H:%M:00Z";
/// Human-readable date, e.g. "1 Jan 2025"
const FMT_NICE: &str = "%-d %b %Y";

// ── Data model ────────────────────────────────────────────────────────────────

/// One blog post / note.  `serde::Serialize` lets minijinja turn it into a
/// template context value without any manual mapping.
#[derive(Debug, Clone, serde::Serialize)]
struct Entry {
    title: String,
    /// Unix timestamp — used only for sorting, not exposed to templates
    #[serde(skip)]
    epoch: i64,
    /// HTML body (markdown already rendered)
    content: String,
    /// Relative URL path, e.g. "2024/my-note"
    url: String,
    /// ISO date for the feed
    feed_date: String,
    /// Pretty date for display
    nice_date: String,
}

/// A minimal (title, url) pair for the prev/next footer links in `nav.j2`.
///
/// This exists purely to avoid passing the *entire* `entries` slice into
/// every note's render context. Without it, `context! { entries }` forces
/// minijinja/serde to serialize every entry's full rendered HTML `content`
/// into a `Value` on every single one of the N page renders — turning an
/// O(n) rendering pass into O(n²) total work as the note count grows.
/// `NavLink` carries only the two fields `nav.j2` actually needs.
#[derive(Debug, Clone, serde::Serialize)]
struct NavLink<'a> {
    title: &'a str,
    url: &'a str,
}

// ── Markdown rendering ────────────────────────────────────────────────────────

/// Convert a Markdown string to HTML using comrak with all required extensions.
///
/// Extensions enabled:
/// * GFM pipe `tables`
/// * `footnotes` ([^1] syntax)
/// * `strikethrough` (~~text~~)
/// * `smart` punctuation — curly quotes, em-dashes (replaces smartypants)
/// * Fenced code blocks are on by default in comrak (CommonMark core)
/// * `unsafe_` — allow raw HTML blocks and inline HTML to pass through
///   unchanged. Without this flag comrak silently replaces every raw HTML
///   element with `<!-- raw HTML omitted -->`. The flag is named `unsafe_`
///   (with trailing underscore) because `unsafe` is a reserved Rust keyword.
fn render_md(text: &str) -> String {
    // NOTE: comrak's `Options<'c>` holds an optional `&mut dyn FnMut` broken-
    // link callback, which makes the type `!Sync` — it cannot be cached in a
    // `static`/`OnceLock` for use across threads. Building it per call is the
    // correct approach here; the struct itself is just a handful of bool
    // flags, so the construction cost is negligible next to the actual
    // markdown parse/render work this function does.
    let mut opts = Options::default();
    opts.extension.table         = true;
    opts.extension.footnotes     = true;
    opts.extension.strikethrough = true;
    opts.parse.smart             = true;  // smartypants (comrak 0.29: ParseOptions)
  //opts.render.unsafe_          = true;  // pass raw HTML through; needed for
                                          // <figure>, <details>, and any inline
                                          // HTML mixed with markdown content
    markdown_to_html(text, &opts)
}

// ── Template preprocessing ────────────────────────────────────────────────────

/// Rewrite `{% markdown %}…{% endmarkdown %}` blocks so minijinja can handle
/// them natively.
///
/// The transform:
/// ```text
/// {% markdown %}          →   {% set _md0 %}
///   {{ expr }} text           {{ expr }} text
/// {% endmarkdown %}       →   {% endset %}{{ _md0 | markdown }}
/// ```
///
/// Because `{% set var %}…{% endset %}` captures the *rendered* content
/// (Jinja expressions already evaluated), the `markdown` filter then receives
/// plain text and produces HTML — exactly what j2m.py's MarkdownExtension did.
///
/// Handles `{%- … -%}` whitespace-trim variants.  Blocks must not be nested.
fn preprocess(src: &str) -> String {
    static OPEN: OnceLock<Regex> = OnceLock::new();
    static CLOSE: OnceLock<Regex> = OnceLock::new();

    let open = OPEN.get_or_init(|| {
        Regex::new(r"\{%-?\s*markdown\s*-?%\}").unwrap()
    });
    let close = CLOSE.get_or_init(|| {
        Regex::new(r"\{%-?\s*endmarkdown\s*-?%\}").unwrap()
    });

    let mut out = src.to_owned();
    let mut n   = 0usize;

    while let Some(om) = open.find(&out) {
        let var = format!("_md{n}");
        n += 1;
        // Replace {% markdown %} with {% set _mdN %}
        out = format!(
            "{}{{% set {var} %}}{}",
            &out[..om.start()],
            &out[om.end()..]
        );
        // Replace {% endmarkdown %} with {% endset %}{{ _mdN | markdown }}
        if let Some(cm) = close.find(&out) {
            out = format!(
                "{}{{% endset %}}{{{{ {var} | markdown }}}}{}",
                &out[..cm.start()],
                &out[cm.end()..]
            );
        }
    }
    out
}

// ── File I/O ──────────────────────────────────────────────────────────────────

/// Write `data` to `www / url_path[.html]`, creating parent directories as
/// needed.  `is_feed = true` suppresses the `.html` extension (used for
/// `feed.json`).
fn write_file(www: &Path, url_path: &str, data: &str, is_feed: bool) {
    let base = www.join(url_path);
    let dest = if is_feed {
        base
    } else {
        base.with_extension("html")
    };

    // Year directories are pre-created once in `ensure_year_dirs` before any
    // notes are written, so this only needs a fallback for `index.html` and
    // `feed.json`, which live directly under `www` (parent already exists).
    // `create_dir_all` on an existing directory is a cheap no-op check, kept
    // here only as a safety net for paths `ensure_year_dirs` didn't see.
    if let Some(parent) = dest.parent() {
        if !parent.exists() {
            if let Err(e) = fs::create_dir_all(parent) {
                eprintln!("  mkdir '{}': {e}", parent.display());
                return;
            }
        }
    }
    if let Err(e) = fs::write(&dest, data.as_bytes()) {
        eprintln!("  write '{}': {e}", dest.display());
    }
}

// ── Note parsing ──────────────────────────────────────────────────────────────

type AnyError = Box<dyn std::error::Error + Send + Sync>;

/// Parse one `.md` note file into an `Entry`.
///
/// Expected file format (identical to the Python version):
/// ```
/// Title text              ← line 1
/// 2024-07-01 09:00        ← line 2 (date, FMT_IN)
///                         ← optional blank line
/// Body in Markdown…       ← rest of file
/// ```
fn parse_entry(path: &Path) -> Result<Entry, AnyError> {
    let raw = fs::read_to_string(path)?;

    // Split into at most three parts: title / date / body
    let mut parts    = raw.splitn(3, '\n');
    let title        = parts.next().unwrap_or("").trim().to_owned();
    let date_str     = parts.next().unwrap_or("").trim().to_owned();
    let body         = parts.next().unwrap_or("");

    let dt    = NaiveDateTime::parse_from_str(&date_str, FMT_IN)
        .map_err(|e| format!("bad date '{}': {e}", date_str))?;
    let epoch = dt.and_utc().timestamp();
    let stem  = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    Ok(Entry {
        title,
        epoch,
        content:   render_md(body),
        url:       format!("{}/{}", dt.format("%Y"), stem),
        feed_date: dt.format(FMT_FEED).to_string(),
        nice_date: dt.format(FMT_NICE).to_string(),
    })
}

// ── Parallel tree walk ────────────────────────────────────────────────────────

/// Walk `source`, parse every `.md` / `.mdown` file in parallel (via rayon),
/// and return entries sorted newest-first.
///
/// The borrow checker guarantees that all `Entry` values are fully owned
/// before they leave this function — no shared mutable state, no data races.
fn get_tree(source: &Path) -> Vec<Entry> {
    // Collect paths first (WalkDir is sequential; parallelism follows)
    let paths: Vec<PathBuf> = WalkDir::new(source)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy();
            (name.ends_with(".md") || name.ends_with(".mdown"))
                && !name.starts_with('.')
        })
        .map(|e| e.into_path())
        .collect();

    // Parse in parallel — rayon distributes across all available cores
    let mut entries: Vec<Entry> = paths
        .par_iter()
        .filter_map(|p| {
            parse_entry(p)
                .map_err(|e| eprintln!("  skip '{}': {e}", p.display()))
                .ok()
        })
        .collect();

    // Sort newest-first; break ties by URL (descending) for stability
    entries.sort_unstable_by(|a, b| {
        b.epoch.cmp(&a.epoch).then_with(|| b.url.cmp(&a.url))
    });
    entries
}

// ── minijinja environment ─────────────────────────────────────────────────────

/// Build a minijinja `Environment` with:
/// * a file-system loader that preprocesses `{% markdown %}` blocks
/// * a `markdown`  filter — `{{ text | markdown }}` -> HTML
/// * an `age`      filter — `{{ birth_month | age(birth_year) }}` -> integer years
/// * a `striptags` filter — `{{ html | striptags }}` -> plain text
/// * a `wordcount` filter — `{{ text | wordcount }}` -> integer word count
/// * a `truncate`  filter — `{{ text | truncate(n) }}` -> truncated string
///
/// `tojson` is provided by minijinja's `json` feature (enabled in Cargo.toml).
/// All other filters (`title`, `e`/`escape`, `selectattr`, `first`, `join`, ...)
/// come from minijinja's `builtins` feature.
fn build_env(tmpl_dir: Arc<PathBuf>) -> Environment<'static> {
    let mut env = Environment::new();

    // -- Loader --------------------------------------------------------------
    // The closure is `'static + Send + Sync` because it only captures an Arc.
    env.set_loader(move |name: &str| -> Result<Option<String>, Error> {
        let path = tmpl_dir.join(name);
        match fs::read_to_string(&path) {
            Ok(src) => Ok(Some(preprocess(&src))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::new(
                ErrorKind::InvalidOperation,
                e.to_string(),
            )),
        }
    });

    // -- Filters -------------------------------------------------------------

    // {{ some_string | markdown }} -- used after {% set %} capture blocks
    env.add_filter("markdown", |s: String| render_md(&s));

    // {{ birth_month | age(birth_year) }}
    env.add_filter("age", |month: u32, year: i32| -> i32 {
        let now = Local::now();
        now.year() - year - i32::from(now.month() < month)
    });

    // {{ html_string | striptags }} -- remove all HTML/XML tags, keep text.
    // Used in detail.j2 for the <meta description> and feed.j2 content_text.
    env.add_filter("striptags", |s: String| -> String {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| Regex::new(r"(?s)<[^>]*>").unwrap());
        // Single pass: strip tags, then collapse whitespace by writing
        // directly into one output buffer (no intermediate Vec<&str>).
        let stripped = re.replace_all(&s, " ");
        let mut out = String::with_capacity(stripped.len());
        let mut last_was_space = true; // trims leading whitespace for free
        for ch in stripped.chars() {
            if ch.is_whitespace() {
                if !last_was_space {
                    out.push(' ');
                    last_was_space = true;
                }
            } else {
                out.push(ch);
                last_was_space = false;
            }
        }
        if out.ends_with(' ') {
            out.pop();
        }
        out
    });

    // {{ text | wordcount }} -- count whitespace-separated words.
    // Used in archive.j2 / latest.j2:  {{ entry.content | wordcount // 250 + 1 }} min
    env.add_filter("wordcount", |s: String| -> usize {
        s.split_whitespace().count()
    });

    // {{ text | truncate }}       -- truncate to 255 chars (default)
    // {{ text | truncate(100) }}  -- truncate to explicit char limit
    // Cuts at the last word boundary before the limit and appends U+2026 (…).
    env.add_filter("truncate", |s: String, length: Option<usize>| -> String {
        let limit = length.unwrap_or(255);
        let end   = '\u{2026}'; // horizontal ellipsis

        // Walk char boundaries directly — no Vec<char> materialisation.
        // `nth_boundary` is the byte offset right after the `limit`-th char.
        let mut char_count = 0usize;
        let mut nth_boundary = s.len();
        for (count, (idx, _)) in s.char_indices().enumerate() {
            char_count = count + 1;
            if count == limit {
                nth_boundary = idx;
                break;
            }
        }
        if char_count <= limit {
            return s; // already short enough, return without copying
        }

        // Reserve one char for the ellipsis: cut one char earlier.
        let budget_end = s[..nth_boundary]
            .char_indices()
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        let budget = &s[..budget_end];
        let cut = budget.rfind(char::is_whitespace).unwrap_or(budget.len());

        let mut out = String::with_capacity(cut + end.len_utf8());
        out.push_str(budget[..cut].trim_end());
        out.push(end);
        out
    });

    env
}

// ── Generation steps ──────────────────────────────────────────────────────────

/// A small macro that wraps a block in a closure returning `Result`, prints
/// "Generating X..." before and "done." / "failed: …" after — matching the
/// Python decorator pattern.
macro_rules! step {
    ($label:literal, $body:block) => {{
        print!("\tGenerating {}...", $label);
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            $body
            Ok(())
        })();
        match result {
            Ok(_)  => println!("done."),
            Err(e) => println!("failed: {e}"),
        }
    }};
}

/// Generate `index.html` (the home page).
fn step_home(entries: &[Entry], env: &Environment, www: &Path) {
    step!("home", {
        let html = env
            .get_template("home.j2")?
            .render(context! { entries })?;
        write_file(www, "index", &html, false);
    });
}

/// Pre-create every distinct `<year>/` output directory once, up front.
///
/// Without this, `write_file` would call `create_dir_all` once per note
/// (58 calls for 58 notes even though there are only a handful of distinct
/// years). Deduplicating first turns that into one `create_dir_all` per
/// unique year — a handful of syscalls instead of dozens.
fn ensure_year_dirs(entries: &[Entry], www: &Path) {
    use std::collections::HashSet;
    let years: HashSet<&str> = entries
        .iter()
        .filter_map(|e| e.url.split('/').next())
        .collect();
    for year in years {
        if let Err(e) = fs::create_dir_all(www.join(year)) {
            eprintln!("  mkdir '{}': {e}", www.join(year).display());
        }
    }
}

/// Generate one HTML page per note (`<year>/<slug>.html`).
///
/// Rendering and writing are parallelised with rayon: `Template::render` only
/// reads the compiled instructions and the per-call context, so multiple
/// notes can be rendered concurrently from a single shared `&Environment`.
/// minijinja's template cache (`MemoMap`) is itself safe for concurrent
/// lookups, so this requires no locking on our side — the borrow checker
/// confirms `&Environment` and `&Template` are `Sync` simply by allowing this
/// code to compile under `par_iter`.
fn step_notes(entries: &[Entry], env: &Environment, www: &Path) {
    step!("notes", {
        let tmpl = env.get_template("detail.j2")?;
        // `entries` are newest-first, so index+1 is the *older* (Prev) note
        // and index-1 is the *newer* (Next) note. Each render gets only
        // tiny `NavLink { title, url }` structs for its neighbours — never
        // the full `entries` slice. Passing the whole slice into context
        // would force serde/minijinja to serialize every entry's full
        // rendered HTML `content` into a `Value` on every single page
        // render, turning an O(n) pass into O(n²) total work as note count
        // grows. With only two small structs per render, this step is O(n).
        entries.par_iter().enumerate().for_each(|(idx, entry)| {
            let prev = entries.get(idx + 1).map(|e| NavLink { title: &e.title, url: &e.url });
            let next = if idx > 0 { entries.get(idx - 1).map(|e| NavLink { title: &e.title, url: &e.url }) } else { None };
            match tmpl.render(context! { entry, prev, next }) {
                Ok(html) => write_file(www, &entry.url, &html, false),
                Err(e)   => eprintln!("  '{}': {e}", entry.url),
            }
        });
    });
}

/// Generate `feed.json` (JSON Feed 1.1) for the `SHOW` most recent entries.
fn step_feed(entries: &[Entry], env: &Environment, www: &Path) {
    step!("feed", {
        let subset = &entries[..entries.len().min(SHOW)];
        let json   = env
            .get_template("feed.j2")?
            .render(context! { entries => subset })?;
        write_file(www, "feed.json", &json, true);
    });
}

// ── Help / usage text ─────────────────────────────────────────────────────────

/// Print `chisel --help` / `-h` output in the conventional Unix man-page
/// layout (NAME, SYNOPSIS, DESCRIPTION, FILES, NOTE FORMAT, AUTHOR).
/// Paths shown below are derived from the same POSTS / WWW / TMPL constants
/// used at runtime, so this text can never drift out of sync with behaviour.
fn print_help() {
    println!(
        "\
NAME
    chisel -- A simple static site generator

SYNOPSIS
    chisel [-h | --help]

DESCRIPTION
    chisel walks a folder of dated Markdown notes, renders each one through
    a set of Jinja2-style (minijinja) templates, and writes out a static
    HTML site plus a JSON feed. It takes no arguments; all paths are fixed
    relative to $HOME (see FILES below).

FILES
    All locations are relative to $HOME (~/).

    ~/{posts}
        Source notes. chisel recursively walks this folder for files
        ending in .md or .mdown (dotfiles are skipped) and parses each
        one into a page.

    ~/{www}
        Output folder. The rendered site is written here:
          index.html          home page
          <year>/<slug>.html  one page per note, grouped by year
          feed.json           JSON Feed (most recent {show} notes)

    ~/{tmpl}
        Template folder. minijinja templates live here (base.j2, home.j2,
        detail.j2, archive.j2, nav.j2, feed.j2, sitesettings.j2, and any
        Markdown includes such as bio.j2, proj.md, colophon.md, etc.).

NOTE FORMAT
    Each note is a plain-text file (.md or .mdown) with three parts, in
    order, separated by newlines:

        Title text              <- line 1
        {date_fmt}          <- line 2 (date, parsed with this format)
                                <- optional blank line
        Body in Markdown…       <- remainder of the file

    Example:

        Set python up the easy way in Windows 11
        2025-03-14 09:00

        Body text with **Markdown** formatting, [links](https://example.com),
        and so on.

    The output URL for a note is <year>/<file-stem>, e.g. a file named
    python-on-windows.md dated 2025-03-14 becomes /2025/python-on-windows.

AUTHOR
    Written by Chetan Kunté
    https://ckunte.net

REPORTING BUGS
    Report issues to: <ckunte@gmail.com>
",
        posts = POSTS,
        www = WWW,
        tmpl = TMPL,
        show = SHOW,
        date_fmt = FMT_IN,
    );
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return;
    }

    let home_dir  = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let loc_posts = home_dir.join(POSTS);
    let loc_www   = home_dir.join(WWW);
    let loc_tmpl  = Arc::new(home_dir.join(TMPL));

    println!("Chiseling...");

    // ── Read & parse notes in parallel ──────────────────────────────────────
    print!("\tReading files...");
    let entries = get_tree(&loc_posts);
    println!("done. ({} notes)", entries.len());

    // ── Set up the template environment ─────────────────────────────────────
    let env = build_env(Arc::clone(&loc_tmpl));

    // ── Run generation steps ─────────────────────────────────────────────────
    ensure_year_dirs(&entries, &loc_www);   // one mkdir per distinct year
    step_home (&entries, &env, &loc_www);
    step_notes(&entries, &env, &loc_www);   // rendered + written in parallel
    step_feed (&entries, &env, &loc_www);
    println!("done.");
}
