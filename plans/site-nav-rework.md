# Site nav: drop the broken egui link, upgrade the site's "Web" link

## Context

`site_nav` (egui, drawn in the wasm build's canvas) used to link back to
lightphotos.app via `hyperlink_to`. A debugging session earlier this
conversation traced why the click never actually opened a tab (webbrowser's
wasm backend, popup-activation, missing wasm logger) but the click path
stayed unconfirmed/unfixed and isn't worth chasing further — the user's call
is to drop the link entirely and just render the wordmark as styled text,
matching the real site's own CSS treatment of "LightPhotos" (plain "Light" +
italic blue "Photos").

Separately, on the real site (lightphotos.app, a sibling static-HTML repo),
the nav's "Web" link (→ `app.html`, the wasm app) should become an explicit,
new-tab, icon-marked "Open LightPhotos in Browser" link — clearer than the
bare "Web" label, and correct now that the app doesn't link back to the site
itself. That link is duplicated by hand across 4 pages today (no build
tooling in that repo at all) — since this edit has to touch that duplicated
markup anyway, extract it into a shared partial assembled by a small new
build script, matching the "build then commit rendered output" pattern the
repo already uses for the wasm bundle (`app.html`'s own comment / this
repo's `scripts/deploy-web.sh`).

Both API choices below (`Color32::PLACEHOLDER`, `TextFormat.italics`,
`LayoutJob::append` signature, `impl From<LayoutJob> for WidgetText`) were
verified directly against the installed `egui-0.34.3`/`epaint-0.34.3` source
in this session, not assumed.

## Repo 1: `/Users/andyyao/lightphotos` — `src/ui/mod.rs`

### New `theme` module constant

Add to the existing `mod theme { ... }` block (`src/ui/mod.rs:21-44`), after
`EYES_BADGE`:

```rust
/// The site wordmark's "Photos" run (italic, blue) — matches
/// lightphotos.app's `--lp-accent` custom property, dark-theme value
/// (`lp.css:11`). The site paints that text with a CSS gradient
/// (`background-clip: text`) that egui has no equivalent for, so this
/// is a flat stand-in for the gradient's dominant color; keep it in
/// sync with `lp.css` if that value ever changes.
pub const BRAND_BLUE: Color32 = Color32::from_rgb(79, 140, 255);
```

### Rewrite `site_nav` (replaces `src/ui/mod.rs:280-309`, doc comment + fn)

```rust
/// The lightphotos.app marketing site's own nav, redrawn in egui so it's
/// consistent even though this page lives inside the wasm canvas rather than
/// the site's plain-HTML chrome (the canvas wants the full viewport —
/// `overflow: hidden` — so wrapping it in the site's HTML header wasn't an
/// option). Persistent across every screen (landing page, Grid, Loupe), per
/// direct request — called once from `draw`'s own top, before the
/// landing-page early return. Just the "LightPhotos" wordmark, styled to
/// match the real site's own CSS treatment (`lp.css`'s `.lp-wordmark`/
/// `.lp-brand` rules: "Light" in the default ink color, "Photos" italic in
/// the brand blue, abutting with no gap) via a two-section `LayoutJob`; no
/// Downloads/Blogs/Help. Plain, non-interactive text, not a link — an
/// earlier version linked back to lightphotos.app via `hyperlink_to`, but
/// the click never actually opened a tab on web and wasn't worth chasing
/// further, so the link was dropped.
fn site_nav(ui: &mut egui::Ui) {
    egui::TopBottomPanel::top("lp_site_nav").show_inside(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            let font_id = egui::TextStyle::Heading.resolve(ui.style());
            let mut job = egui::text::LayoutJob::default();
            job.append(
                "Light",
                0.0,
                egui::TextFormat {
                    font_id: font_id.clone(),
                    // PLACEHOLDER = "not explicitly colored"; egui's
                    // text-shape painter substitutes the widget's normal
                    // text color for any PLACEHOLDER glyph at paint time
                    // — exactly "inherit the default ink color" with no
                    // color logic of our own to keep in sync.
                    color: egui::Color32::PLACEHOLDER,
                    ..Default::default()
                },
            );
            job.append(
                "Photos",
                0.0,
                egui::TextFormat {
                    font_id,
                    color: theme::BRAND_BLUE,
                    italics: true,
                    ..Default::default()
                },
            );
            ui.label(job);
        });
        ui.add_space(4.0);
    });
}
```

Notes:
- `LayoutJob`/`italics` are new to this crate (no prior usage) — self-contained first use, no other call sites to touch.
- `theme::BRAND_BLUE` is in scope already (same file).
- `ui.label(job)` works directly via `impl From<LayoutJob> for WidgetText` — no `egui::Label::new(job)` wrapper needed.
- After this change: `grep -rn "hyperlink" src/` → zero matches.

## Repo 2: `/Users/andyyao/lightphotos.app` — dedupe nav + change the "Web" link

All 4 content pages (`index.html`, `downloads.html`, `blogs.html`,
`docs.html`) were read in full this session. Confirmed: `<head>` boilerplate
is byte-identical except `<title>`/`<meta description>` (2 lines); the
`<header class="lp-site-header">` block is byte-identical except which one
nav `<a>` carries `class="lp-active"`; the footer
(`<script src="./lp-theme.js"></script></body></html>`) is byte-identical.
`app.html` and the gitignored `wasm-decode-probe.html` have no nav — not
touched. Per explicit decision, `index.html`'s separate hero-card "Web →"
link (its own heading+paragraph, a different element) is **not** touched —
only the shared nav link changes.

### New layout

```
lightphotos.app/
  partials/
    head.html             (shared <head>, __TITLE__/__DESCRIPTION__ tokens)
    nav.html               (shared header nav incl. new "Open LightPhotos
                             in Browser" link + icon; __ACTIVE_DOWNLOADS__/
                             __ACTIVE_BLOGS__/__ACTIVE_DOCS__ tokens)
    footer.html             (shared closing boilerplate)
    body-index.html          (index.html's <main>...</main>, verbatim)
    body-downloads.html      (downloads.html's <main>...</main>, verbatim)
    body-blogs.html          (blogs.html's <main>...</main>, verbatim)
    body-docs.html           (docs.html's <main>...</main>, verbatim)
  scripts/
    build-site.sh             (assembles the 4 pages from the partials above)
  index.html / downloads.html / blogs.html / docs.html   (generated, still committed)
  lp.css                      (edit: + .lp-ext-icon rule)
```

Design calls made: title/description live as inline arguments in
`build-site.sh` itself (4 known pages, not worth a front-matter parsing
convention for 2 strings); active-link state is 3 sed-substituted tokens
(no `__ACTIVE_WEB__` — the "Web"/"Open LightPhotos..." link is never
`lp-active` in any current page, confirmed).

### `partials/head.html` (new)

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<script>
(function () {
  try {
    var t = localStorage.getItem('lp-theme');
    if (t === 'light' || t === 'neutral' || t === 'dark') {
      document.documentElement.setAttribute('data-theme', t);
    }
  } catch (e) {}
})();
</script>
<meta name="viewport" content="width=device-width, initial-scale=1" />
<meta name="description" content="__DESCRIPTION__" />
<title>__TITLE__</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Fraunces:opsz,wght@9..144,400;9..144,560&family=Inter:wght@400;500&display=swap" rel="stylesheet">
<link rel="stylesheet" href="./lp.css">
</head>
```

### `partials/nav.html` (new) — includes the "Web" link change + icon

```html
<body>
  <header class="lp-site-header">
    <a class="lp-brand" href="/">Light<span class="lp-light">Photos</span></a>
    <div class="lp-header-right">
    <nav aria-label="Main">
      <a href="./app.html" target="_blank" rel="noopener">Open LightPhotos in Browser<svg class="lp-ext-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" focusable="false"><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/><polyline points="15 3 21 3 21 9"/><line x1="10" y1="14" x2="21" y2="3"/></svg></a>
      <a href="./downloads.html"__ACTIVE_DOWNLOADS__>Downloads</a>
      <a href="./blogs.html"__ACTIVE_BLOGS__>Blogs</a>
      <a href="./docs.html"__ACTIVE_DOCS__>Help</a>
    </nav>
    <div class="lp-theme-toggle" role="group" aria-label="Theme">
      <button type="button" data-theme-choice="light" aria-label="Light theme" aria-pressed="false">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41"/></svg>
      </button>
      <button type="button" data-theme-choice="neutral" aria-label="Neutral gray theme" aria-pressed="false">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="9"/><path d="M12 3a9 9 0 0 1 0 18z" fill="currentColor" stroke="none"/></svg>
      </button>
      <button type="button" data-theme-choice="dark" aria-label="Dark theme" aria-pressed="true">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z"/></svg>
      </button>
    </div>
    </div>
  </header>

```

(Must keep the trailing blank line after `</header>` — matches the blank
line separating `</header>` from `<main>` in every current source file.)

Icon: standard "external link" glyph (box + arrow escaping the corner),
same stroke convention as every other icon in this repo (`viewBox="0 0 24
24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"
stroke-linejoin="round"`). `aria-hidden="true" focusable="false"` since it's
decorative — the link text already states the destination. (Follow-up, not
in scope now: a visually-hidden "(opens in new tab)" span would need a new
`.lp-sr-only` utility class that doesn't exist yet — not adding speculatively.)

### `partials/footer.html` (new)

```html
<script src="./lp-theme.js"></script>
</body>
</html>
```

### `partials/body-*.html` (new, 4 files)

Verbatim `<main>...</main>` extracted from each current page — no content
changes. `body-index.html` keeps the hero-card grid (including the
untouched "Web →" card) exactly as it is today; `body-downloads.html`,
`body-blogs.html`, `body-docs.html` keep their current "TBD" placeholder
content exactly as-is.

### `scripts/build-site.sh` (new, chmod +x)

```bash
#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Assembles index.html, downloads.html, blogs.html and docs.html from the
# shared partials/ (head boilerplate, site nav, footer) plus one
# per-page partials/body-*.html, so the <head> boilerplate and the
# <header class="lp-site-header"> nav don't have to be hand-edited in all
# four files every time either changes. Run this, review the diff, commit
# the rendered HTML — this repo still has no other build tooling and none
# is being added; same "build then commit rendered output" spirit as the
# sibling lightphotos repo's scripts/deploy-web.sh, just for the nav
# instead of the wasm bundle.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PARTIALS="$ROOT/partials"

render_head() {
  local title="$1" desc="$2"
  sed -e "s|__TITLE__|${title}|" -e "s|__DESCRIPTION__|${desc}|" "$PARTIALS/head.html"
}

render_nav() {
  # $1/$2/$3: "active" or "" for the downloads/blogs/docs nav links, in
  # that order. The "Open LightPhotos in Browser" link never carries
  # lp-active (app.html has no nav of its own to link back from), so it
  # has no placeholder at all.
  local dl_attr="" bl_attr="" dc_attr=""
  [[ "$1" == "active" ]] && dl_attr=' class="lp-active"'
  [[ "$2" == "active" ]] && bl_attr=' class="lp-active"'
  [[ "$3" == "active" ]] && dc_attr=' class="lp-active"'
  sed \
    -e "s|__ACTIVE_DOWNLOADS__|${dl_attr}|" \
    -e "s|__ACTIVE_BLOGS__|${bl_attr}|" \
    -e "s|__ACTIVE_DOCS__|${dc_attr}|" \
    "$PARTIALS/nav.html"
}

build_page() {
  local out="$1" title="$2" desc="$3" body="$4" dl="$5" bl="$6" dc="$7"
  echo "==> Building $out"
  {
    render_head "$title" "$desc"
    render_nav "$dl" "$bl" "$dc"
    cat "$PARTIALS/$body"
    cat "$PARTIALS/footer.html"
  } > "$ROOT/$out"
}

echo "==> Assembling site pages from partials/"

build_page "index.html" \
  "LightPhotos — Every photo, seen in its best light" \
  "LightPhotos — a fast, private photo editor that runs in your browser or as a desktop app." \
  "body-index.html" "" "" ""

build_page "downloads.html" \
  "Download LightPhotos" \
  "Download LightPhotos — under construction. Details to be determined." \
  "body-downloads.html" "active" "" ""

build_page "blogs.html" \
  "LightPhotos Blog" \
  "LightPhotos Blog — under construction. Details to be determined." \
  "body-blogs.html" "" "active" ""

build_page "docs.html" \
  "LightPhotos Documentation" \
  "LightPhotos Documentation — under construction. Details to be determined." \
  "body-docs.html" "" "" "active"

echo "==> Done. Review the diff before committing:"
echo "    cd $ROOT && git diff -- index.html downloads.html blogs.html docs.html"
```

Fragility flagged, not fixed: plain `sed` string substitution — a future
title/description containing `&`, `\`, or `|` would corrupt output. None of
the current 4 do; the `git diff` verification step is the safety net.

### `lp.css` edit — new `.lp-ext-icon` rule

Insert after the existing `.lp-site-header nav a + a::before` rule
(currently `lp.css:164-168`), before the `.lp-page` rule:

```css
.lp-site-header nav a .lp-ext-icon {
  width: 1em;
  height: 1em;
  margin-left: 0.3em;
  vertical-align: -0.15em;
}
```

`em`-sized so it scales with the nav link's `font-size: 0.85rem` (~13.6px
rendered). No color rule needed — the SVG's `stroke="currentColor"`
inherits the nav link's existing `--lp-muted` → `--lp-ink` hover transition
automatically, in every theme.

## Verification

**Repo 1 (egui):**
1. `cd /Users/andyyao/lightphotos && RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk serve --config Trunk.toml` (a dev server was already reachable at `localhost:8765` earlier this session).
2. In the browser: confirm "Light" renders in the default egui text color, "Photos" abuts with no gap, is italic, and is flat blue (~`#4f8cff`); hovering shows no pointer cursor/underline; clicking does nothing.
3. `grep -rn "hyperlink" src/` → zero matches.
4. `cargo check --target wasm32-unknown-unknown` → clean build, no new warnings.

**Repo 2 (static site):**
1. `cd /Users/andyyao/lightphotos.app && git status` (clean tree first, so the diff below is only this change).
2. `bash scripts/build-site.sh`
3. `git diff -- index.html downloads.html blogs.html docs.html` → expect exactly one changed line per file (the nav link). Anything else in the diff = partial-extraction bug — fix the source partial, don't hand-patch the generated files.
4. `git diff --stat` → expect `4 files changed, 4 insertions(+), 4 deletions(-)`.
5. Open `index.html` locally, click "Open LightPhotos in Browser" → opens `app.html` in a new tab; icon visible inline; icon color follows the light/neutral/dark theme toggle.
6. Re-run `bash scripts/build-site.sh` with no source changes → `git diff` empty (idempotent).

### Critical files

- `/Users/andyyao/lightphotos/src/ui/mod.rs`
- `/Users/andyyao/lightphotos.app/partials/nav.html`
- `/Users/andyyao/lightphotos.app/partials/head.html`
- `/Users/andyyao/lightphotos.app/scripts/build-site.sh`
- `/Users/andyyao/lightphotos.app/lp.css`
