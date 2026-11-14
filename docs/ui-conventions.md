# UI conventions

Moved out of `CLAUDE.md` to keep agent context small; the short rules there are binding, this has the reasons and details.

## UI changes use Apple App and Website styles

Every UI change (dashboard pages, `crates/server/assets/*`, the macOS app) must look
like Apple's apps and website: system font stack (`-apple-system`, `SF Pro`,
`ui-monospace` for code), hairline separators, 12-16px rounded corners, soft shadows,
translucent blurred material for sheets/sidebars/inspectors, one system-blue accent
with neutral grays, pill buttons and segmented controls, eased 200-350ms transitions
(`cubic-bezier(0.32, 0.72, 0, 1)`), full light/dark parity. Prefer a sheet or right-hand
inspector panel over a new tab or a browser popup. Keep the dashboard's `--md-*` tokens
as the color source, and keep functional motion (the loading spinner) under reduced
motion. The `impeccable` and `ui-ux-pro-max` skills carry the same rule.

## Never ship UI you have not rendered: forms, inputs and reused classes

A UI change is not done until you have looked at it rendered, in every state it can be in
(collapsed and expanded sections, tracked and untracked, empty and filled, light and dark).
`cargo test` and the golden snapshots only prove the markup did not change unexpectedly; they
cannot see a 270px-tall text box. Render it for real: build, run the sandbox from
"Development mode" with seeded config, fetch the page or its lazy fragment
(`/settings/fragment`, `/gitlab/live`), open the `<details>` you changed, and take a headless
Chrome screenshot (`--headless=new --screenshot=...`). Read the screenshot before saying done.

Layout-coupled classes must not be reused in a different layout. `.daemon-action-form` sizes
its inputs with `flex: 1 1 160px`, which is a *width* in a row and a *160px+ height* in a
column; the Projects cards once shipped with giant inputs for exactly this reason. For a new
layout, write that layout's own classes (`.proj-form`, `.proj-field`: explicit `height`,
`width: 100%`, `box-sizing`, border, radius, font) instead of overriding a row form's CSS, and
reset `flex` on every child if you must reuse one. Every input, select, textarea and button
has an explicit height and the dashboard's `--md-*` tokens.

Forms follow one rule: one form, one Save. Do not give a card two Save buttons or two controls
for the same setting (a switch and a checkbox for `use_worktree`); put the immediate-effect
toggle where it is used and keep the rest in a single labelled form.

## Adding an icon

Icons are an inline SVG sprite (`crates/server/assets/icons/sprite.svg`, Lucide, ISC
licence) rendered by `loopx_server::ui::icons::icon(name)`. Ids keep the old Material
glyph names, so `icon("settings")` is `<use href='#i-settings'/>`. To add one, append a
`<symbol id="i-NAME" viewBox="0 0 24 24">` to the sprite, add `NAME` to
`icons::NAMES`, and update the golden snapshots (`LOOPX_GOLDEN_BLESS=1 cargo test -p
loopx-server --test golden_ui -- --test-threads=1`). An unknown name renders the
fallback circle, and the golden test fails on any dangling `#i-…` reference.

## A loading spinner's motion is functional, not decorative

Purely decorative animations (`.pulse-dot`, the topbar progress sliver) are gated
behind `@media (prefers-reduced-motion: no-preference)`, and should stay that way. A
*loading* spinner is different: its motion is the only signal a page is still working,
so `.md-spinner`'s animation is deliberately **not** gated — under
`prefers-reduced-motion`, it would otherwise look permanently frozen rather than just
calmer. Don't move it back inside that media block.

## Flexbox: watch for the shrink-and-clip trap

A flex child with `flex-shrink: 0` forces *all* the shrinkage onto its sibling(s). If
that sibling also has `overflow: hidden`, it can be crushed to near-zero width and its
content effectively disappears — even though every `display`/visibility property on it
is technically correct. This bit the collapsed sidebar once (the brand icon vs. the
toggle button competing for a 64px rail); if a collapsed/narrow layout ever "loses" an
element that has `display` set correctly, check the flex-shrink math on its container
before touching `display` again.

## Page sections: let the shared card spacing do it

`.card` has no margin by design. Sibling cards at page level (direct children of
`.wrap`) get their 1.25rem gap from the shared `.wrap > .card + *` rule; cards inside a
container go in `.grid` or a flex stack that sets `gap`. Never add per-page margin
hacks, and never wrap a new view's cards in an extra div that isn't `.grid`/a gap
stack — that silently opts out of the shared rule. This bit Connectors → Accounts and
Harness → Gates.
