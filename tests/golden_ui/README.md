# Golden UI harness

The Python dashboard (`bin/web/dashboard_server.py`) is the byte-level oracle for
the Rust pages in `crates/server/src/ui/`.

* `capture.py` runs the **Python** dashboard in a sandbox (scratch copy of `bin/`,
  scratch `HOME` / `LOOP_ENGINEERING_HOME` / `LOOP_UPDATE_DIR` / `LOOP_EVENTS_DIR`,
  frozen clock from `clock.txt`, CSRF token pinned to `golden-csrf-token`) against
  `fixture/` (+ an optional `overlays/<name>/`), requests every entry of
  `pages.json` in en/ja/zh/fr, and writes `expected/` (checked in).
  It also dumps `render_shell()` for every entry of `shell_variants.json`.
* `crates/server/tests/golden_ui.rs` builds the same fixture in a temp dir, runs each
  page through the Rust registry in-process and diffs. A page no Rust route claims is
  **pending** (it still goes to the legacy proxy in production), not failed.

```
python3 tests/golden_ui/capture.py                 # regenerate everything
python3 tests/golden_ui/capture.py --only readme   # just these page ids
cargo test -p loopx-server --test golden_ui -- --nocapture   # prints ported/pending per language
LOOPX_GOLDEN_FILTER=readme,update LOOPX_GOLDEN_VERBOSE=1 cargo test -p loopx-server --test golden_ui -- --nocapture
```

## Normalization (the complete list)

1. `127.0.0.1:<port>` becomes `127.0.0.1:PORT` on both sides.
2. The sandbox HOME path `<scratch>/userhome` becomes `/USERHOME` on both sides (the Skills page
   prints paths under it).
3. Large `<style>` / `<script>` bodies are stored once in `expected/_blobs/` and
   referenced as `<!--@blob:HASH-->`; the test expands them first (storage only).

Everything else is pinned, not normalized: the clock, the CSRF token, fixture file
mtimes (clock minus 1 h, set by both sides), and `@PID@` in fixture files (replaced by
the capturing / testing process's pid so "is this pid alive" is true on both sides).
If your page prints something the fixture cannot pin, extend the pinning (a launcher
patch in `launch_python.py` and the same in `build_fixture`), and document it here.

## Adding a page (the recipe)

1. **Register the route.** Create `crates/server/src/ui/pages/<yourpage>.rs` with
   `pub fn register(r: &mut Registry)`; `build.rs` picks the file up, so no shared file
   changes. Use `r.get("/path", handler)`, `r.post(..)` (CSRF is checked for you; 403
   exactly like Python), `:param` / `*rest` patterns. Handlers are
   `fn(&UiCtx) -> UiResponse`: `ctx.lang`, `ctx.t("English")` (every literal must be in
   `bin/locales/{ja,zh,fr}.json`; a unit test enforces it), `ctx.query()`, `ctx.form()`,
   `ctx.flash()`, `ctx.csrf_input()`, `ctx.state.{repo_root,home_dir,user_home,now()}`.
   Wrap bodies with `shell::render_shell(ctx, title, active_page, badge, body, ShellOpts)`
   or `hub::render_hub_page` for `?view=` hubs. Redirects with a flash:
   `UiResponse::flash_redirect(ok, msg, location)`.
2. **Get the markup without retyping it.** Add `tools/ui_assets_targets/<yourpage>.py`
   with `TEMPLATES = [("dashboard_server", "render_x_page", "x_page"), ...]` (see
   `tools/extract_ui_assets.py`), run `python3 tools/extract_ui_assets.py`, then in Rust
   `Template::named("x_page").render(&scope)`. Slots like `html.escape(_t('Foo'))` resolve
   by themselves; computed ones are given with `scope.var(..)` / `scope.set(expr, value)`.
   Python constants (icons, tables) are `ui::consts::ds("_NAME")` / `consts::value(..)`.
   `tests/test_extract_ui_assets.py` fails if the checked-in assets drift.
3. **Add fixture data.** Put files the page reads under `fixture/{checkout,home,userhome,update}/`
   (checkout = repo root with `outputs/`, home = `LOOP_ENGINEERING_HOME`, userhome = `$HOME`).
   Data that only one page needs, or alternative states, go in `overlays/<name>/<same dirs>`.
4. **List the URLs** in `pages.json` (`overlay`, `langs` optional). Then run `capture.py`,
   look at the generated `expected/` once, and run the cargo test. Iterate until your page
   moves from "pending" to "ported" in all four languages.
5. Kill switches for debugging against Python: `LOOPX_LEGACY_PAGES=/readme,/update`
   proxies those routes to the legacy dashboard; `LOOPX_PORTED=none` (or a list) limits
   what Rust serves.

Adding fixture files can change *other* pages' output (the sidebar lists registered
loops, the update banner reads `update/`), so re-run the full capture and the full test.
