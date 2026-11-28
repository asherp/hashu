//! The seller's terms of sale, served at `/terms/` from a Markdown file
//! (`--terms-file`, `HASHU_TERMS_FILE`).
//!
//! The file is the operator's own text: the repository ships Hashu's at
//! `deploy/terms/seller-terms.md`, and other operators point this at theirs.
//! It is read and rendered once at startup, so a missing or unreadable file
//! fails fast instead of serving a broken page. Raw HTML in the file is
//! shown as text, never passed through.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::response::{Html, Redirect};
use axum::routing::get;
use pulldown_cmark::{html, Event, Options, Parser};

/// Where the terms are served on the mint's host.
pub const PATH: &str = "/terms/";

/// `--terms-file` read and rendered as a full HTML page.
pub fn load(path: &Path) -> Result<String> {
    let md = std::fs::read_to_string(path)
        .with_context(|| format!("read --terms-file {}", path.display()))?;
    anyhow::ensure!(
        !md.trim().is_empty(),
        "--terms-file {} is empty",
        path.display()
    );
    Ok(render(&md))
}

/// Markdown terms as a page in the checkout's style.
pub fn render(md: &str) -> String {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    let events = Parser::new_ext(md, opts).map(|e| match e {
        Event::Html(t) | Event::InlineHtml(t) => Event::Text(t),
        e => e,
    });
    let mut body = String::new();
    html::push_html(&mut body, events);
    let title = md
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .unwrap_or("Terms of sale")
        .trim();
    PAGE.replacen("{{TITLE}}", &escape(title), 1)
        .replacen("{{BODY}}", &body, 1)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `/terms` and `/terms/`; nothing when no terms file is set.
pub fn router(page: Option<Arc<String>>) -> axum::Router {
    let Some(page) = page else {
        return axum::Router::new();
    };
    axum::Router::new()
        .route("/terms", get(|| async { Redirect::permanent(PATH) }))
        .route(
            PATH,
            get(move || {
                let page = page.clone();
                async move { Html(page.as_str().to_owned()) }
            }),
        )
}

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{{TITLE}}</title>
<style>
:root { --bg:#f6f6f4; --surface:#fff; --ink:#1b1b1a; --ink-2:#55534f; --line:rgba(27,27,26,.12); --brand:#e8850c; }
@media (prefers-color-scheme: dark) { :root { --bg:#111110; --surface:#1a1a19; --ink:#f1efea; --ink-2:#b5b2ab; --line:rgba(241,239,234,.14); --brand:#f7931a; } }
* { box-sizing:border-box; }
body { margin:0; background:var(--bg); color:var(--ink); font:16px/1.6 system-ui,-apple-system,"Segoe UI",sans-serif; }
main { max-width:760px; margin:0 auto; padding:32px 16px 64px; }
article { background:var(--surface); border:1px solid var(--line); border-radius:12px; padding:8px 24px 24px; overflow-wrap:anywhere; }
h1 { font-size:28px; line-height:1.25; letter-spacing:-.01em; margin:20px 0 8px; }
h2 { font-size:19px; margin:32px 0 8px; }
p, li { color:var(--ink); }
li { margin:6px 0; }
a { color:var(--brand); }
code { font-size:14px; }
.scroll { overflow-x:auto; }
table { width:100%; border-collapse:collapse; font-size:14px; }
th { text-align:left; color:var(--ink-2); font-size:13px; }
th, td { padding:8px 6px; border-bottom:1px solid var(--line); vertical-align:top; }
@media (max-width:560px) { article { padding:4px 16px 16px; } h1 { font-size:24px; } }
</style>
</head>
<body>
<main><article>
{{BODY}}
</article></main>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_markdown_and_escapes_html() {
        let page = render("# Terms of <sale>\n\nHello **you**. <script>x</script>\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n");
        assert!(page.contains("<title>Terms of &lt;sale&gt;</title>"));
        assert!(page.contains("<strong>you</strong>"));
        assert!(page.contains("<table>"));
        assert!(!page.contains("<script>"));
        assert!(page.contains("&lt;script&gt;"));
    }

    #[test]
    fn shipped_terms_render() {
        let md = include_str!("../../deploy/terms/seller-terms.md");
        let page = render(md);
        assert!(page.contains("<title>Hashu Seller Terms of Service</title>"));
        assert!(page.contains("Pembroke Creative, LLC"));
    }

    #[test]
    fn empty_or_missing_file_fails() {
        assert!(load(Path::new("/nonexistent/terms.md")).is_err());
        let f = tempfile::NamedTempFile::new().unwrap();
        assert!(load(f.path()).is_err());
    }
}
