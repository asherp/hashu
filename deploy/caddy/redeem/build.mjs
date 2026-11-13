// Renders the per-pool redemption guides (docs/redeem/*.md) into static
// pages for Caddy to serve at /redeem/ (see deploy/caddy/Dockerfile).
//
//   node build.mjs <docs/redeem dir> <out dir>
//
// README.md becomes index.html and every other foo.md becomes foo.html;
// links between the guides are rewritten to match.

import { readdirSync, readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { join, basename } from 'node:path';
import { marked } from 'marked';

const [src, out] = process.argv.slice(2);
if (!src || !out) {
  console.error('usage: node build.mjs <docs/redeem dir> <out dir>');
  process.exit(2);
}

const pageName = (md) => (md === 'README.md' ? 'index.html' : md.replace(/\.md$/, '.html'));
const pageHref = (md) => (md === 'README.md' ? './' : md.replace(/\.md$/, '.html'));

// Rewrite relative links: sibling guides to their pages, anything else in
// the repo to GitHub, so nothing points at a .md file the site doesn't serve.
const REPO = 'https://github.com/asherp/hashu/blob/main/';
marked.use({
  walkTokens(t) {
    if (t.type !== 'link' || /^(https?:|mailto:|#)/.test(t.href)) return;
    const [path, frag] = t.href.split('#');
    if (/^[\w-]+\.md$/.test(path)) {
      t.href = pageHref(path) + (frag ? '#' + frag : '');
    } else {
      t.href = REPO + 'docs/redeem/' + t.href;
    }
  },
});

const esc = (s) => s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);

const CSS = `
:root {
  --bg: #f6f6f4; --surface: #ffffff; --ink: #1b1b1a; --ink-2: #55534f; --ink-3: #8a8782;
  --line: rgba(27,27,26,0.12); --brand: #e8850c; --brand-fill: rgba(232,133,12,0.12);
  --warn-bg: rgba(232,133,12,0.10);
}
@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --bg: #111110; --surface: #1a1a19; --ink: #f1efea; --ink-2: #b5b2ab; --ink-3: #85827c;
    --line: rgba(241,239,234,0.14); --brand: #f7931a; --brand-fill: rgba(247,147,26,0.14);
    --warn-bg: rgba(247,147,26,0.12);
  }
}
* { box-sizing: border-box; }
body { margin: 0; background: var(--bg); color: var(--ink); font: 16px/1.6 system-ui, -apple-system, "Segoe UI", sans-serif; }
a { color: var(--brand); }
.wrap { max-width: 820px; margin: 0 auto; padding: 0 16px; }
nav { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 18px 0; flex-wrap: wrap; border-bottom: 1px solid var(--line); }
.logo { display: flex; align-items: center; gap: 10px; font-weight: 700; font-size: 18px; text-decoration: none; color: var(--ink); }
.logo svg { width: 28px; height: 28px; }
nav .links { display: flex; gap: 18px; font-size: 14px; flex-wrap: wrap; }
nav .links a { color: var(--ink-2); text-decoration: none; }
nav .links a:hover { color: var(--ink); }
main { padding: 24px 0 48px; }
.crumb { font-size: 14px; color: var(--ink-3); margin: 0 0 8px; }
.crumb a { color: var(--ink-2); }
h1 { font-size: clamp(28px, 5vw, 40px); line-height: 1.1; letter-spacing: -0.02em; margin: 0 0 16px; }
h2 { font-size: 20px; margin: 32px 0 10px; }
p, li { color: var(--ink-2); }
strong { color: var(--ink); }
.table { overflow-x: auto; margin: 16px 0; }
table { border-collapse: collapse; width: 100%; font-size: 15px; background: var(--surface); border: 1px solid var(--line); border-radius: 10px; }
th, td { text-align: left; padding: 9px 12px; border-bottom: 1px solid var(--line); vertical-align: top; }
th { font-size: 13px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--ink-3); font-weight: 600; }
tr:last-child td { border-bottom: 0; }
code { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.88em; background: var(--brand-fill); border-radius: 5px; padding: 1px 5px; overflow-wrap: anywhere; }
pre { background: var(--surface); border: 1px solid var(--line); border-radius: 8px; padding: 12px; overflow-x: auto; }
pre code { background: none; padding: 0; }
blockquote { margin: 16px 0; padding: 10px 16px; background: var(--warn-bg); border-left: 3px solid var(--brand); border-radius: 0 8px 8px 0; }
blockquote p { margin: 4px 0; color: var(--ink); }
footer { padding: 24px 0 40px; border-top: 1px solid var(--line); color: var(--ink-3); font-size: 14px; }
footer a { color: var(--ink-2); }
`;

const LOGO = `<svg viewBox="0 0 32 32" aria-hidden="true"><rect width="32" height="32" rx="7" fill="#f7931a"/><path d="M10 7v18M22 7v18M10 16h12" stroke="#111" stroke-width="3.4" stroke-linecap="round"/></svg>`;
const ICON = `data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='7' fill='%23f7931a'/%3E%3Cpath d='M10 7v18M22 7v18M10 16h12' stroke='%23111' stroke-width='3.4' stroke-linecap='round'/%3E%3C/svg%3E`;

function page({ md, title, body }) {
  const isIndex = md === 'README.md';
  const crumb = isIndex ? '' : `<p class="crumb"><a href="./">Redeeming TiH</a> / ${esc(title.replace(/^Redeem at /, ''))}</p>`;
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)} · Hashu</title>
<link rel="icon" href="${ICON}">
<style>${CSS}</style>
</head>
<body>
<div class="wrap">
  <nav>
    <a class="logo" href="/">${LOGO} Hashu</a>
    <div class="links">
      <a href="./">Pool guides</a>
      <a class="mint-link" data-path="checkout/" href="/checkout/">Buy &amp; redeem</a>
      <a href="https://github.com/asherp/hashu/tree/main/docs/redeem">Edit on GitHub</a>
    </div>
  </nav>
  <main>
    ${crumb}
    ${body}
  </main>
  <footer>Source: <a href="https://github.com/asherp/hashu/blob/main/docs/redeem/${md}">docs/redeem/${md}</a></footer>
</div>
<script>
// Point mint links at this deployment's mint, as the splash page does.
fetch('/checkout/api/info', { cache: 'no-store' })
  .then((r) => (r.ok ? r.json() : null))
  .then((i) => {
    if (!i || !/^https?:\\/\\//.test(i.mint_url || '')) return;
    const base = i.mint_url.replace(/\\/+$/, '') + '/';
    document.querySelectorAll('a.mint-link').forEach((a) => { a.href = base + (a.dataset.path || ''); });
  })
  .catch(() => {});
</script>
</body>
</html>
`;
}

mkdirSync(out, { recursive: true });
const files = readdirSync(src).filter((f) => f.endsWith('.md')).sort();
for (const md of files) {
  const text = readFileSync(join(src, md), 'utf8');
  const title = (text.match(/^#\s+(.+)$/m) || [, basename(md, '.md')])[1].replace(/`/g, '');
  const body = marked.parse(text).replace(/<table>/g, '<div class="table"><table>').replace(/<\/table>/g, '</table></div>');
  writeFileSync(join(out, pageName(md)), page({ md, title, body }));
  console.log(`${md} -> ${pageName(md)}`);
}
