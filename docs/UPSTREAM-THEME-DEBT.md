# Upstream saola-theme debt

Consolidated at the end of saola-files' Stage 16 (polish + release), for
whoever picks up saola-theme work next. Each item below is something
saola-files' CLAUDE.md forbids fixing locally ("zero hardcoded colors or
sizes… if a style is missing, add it to saola-theme — never restyle
locally"), so it's parked here instead of patched around in this repo.

None of these block saola-files v0.1.0 shipping — they're recorded so the
next saola-theme release can pick them up deliberately, not rediscover them
as regressions.

## 1. `button::rest`/`emphasis`'s un-emphasized ink arm reads as a loud ivory chip

**Status:** open design question, known before Stage 16, now visually
confirmed live.

`style::button::emphasis(t, s, active)` (and the plain `rest` recipe
underneath it) paints the *un-emphasized* state as a solid ivory fill on
every surface, including `Surface::Ink`. That's correct for shell chrome —
an ivory pill on an ink panel is exactly the "off" state the design
language wants — but inside an **ink window** (this app's `surface = "ink"`
config knob), it means an unselected control renders as a bright, filled
ivory shape sitting directly on the ink ground, rather than a subtle,
recessed "not active" mark.

Confirmed live 2026-08-13 (saola-files Stage 16, ink surface,
`SAOLA_CONFIG_DIR` pointed at a `surface = "ink"` config): the header's
List/Grid view switcher (`widget::segmented_row`, styled via
`style::button::emphasis`) shows the active "List" segment as the expected
terracotta pill, but the inactive "Grid" segment renders as a solid white
circle — reads as brighter and more attention-grabbing than the terracotta
segment right next to it, inverting the intended visual hierarchy (rest
should recede, active should stand out). The same issue affects the
Hidden-files toggle when it's off inside an ink window.

**Likely fix shape:** `style::button::emphasis`/`rest` need a
window-vs-shell distinction, not just the existing `Surface` (ink/paper)
parameter — an ink *window* control's rest state wants something closer to
the paper window's subtle-fill treatment (`rgba(255,255,240,0.07-0.12)`
stepped fill, no full-opacity ivory), while ink *shell chrome* (panel,
popovers, launcher) keeps today's full-ivory-at-rest behavior. This needs a
saola-theme design decision (a new enum arm? a bool flag on the existing
call? a separate `style::button::window_emphasis`?), not a call-site
workaround in saola-files.

**Where it shows up in saola-files today:** `ui/header.rs`'s
`view_switcher` (List/Grid) and `hidden_toggle`, both via
`style::button::emphasis(t, s, active)`. Not touched this stage per
CLAUDE.md's "never restyle locally."

## 2. `widget::breadcrumb` has no overflow strategy for deep paths

**Status:** new finding, Stage 16 §11 pass. Worked around app-side (see
below); the real fix belongs upstream.

`widget::breadcrumb(t, s, crumbs)` builds a plain `iced::widget::Row` with
no width constraint — every crumb pill is laid out at its natural size, and
the row's total width grows without bound as the trail gets deeper. Unlike
`overflow::truncate`/`overflow::unit_budget` (added in 0.9.0–0.11.0
specifically because grid tile labels and the list view's name column hit
this same class of problem), the breadcrumb trail was never given a
width-budget treatment.

Confirmed live 2026-08-13 (saola-files Stage 16, `§11` pass): navigating to
a directory nested a few levels under a long-named scratch path produced a
breadcrumb trail wider than the space left after the nav
buttons/edit-pencil/List-Grid switcher/Hidden toggle/overflow menu — the
un-clipped `Row` painted straight over those sibling controls instead of
shrinking, truncating, or wrapping. Screenshot evidence:
`breadcrumb-crop.png` from that session shows the "Documents" current-crumb
pill's terracotta fill overlapping the edit-pencil icon glyph.

**App-side stopgap already applied** (`src/ui/breadcrumbs.rs::pills`):
`widget::breadcrumb`'s output is wrapped in a horizontal
`iced::widget::scrollable` (`style::scrollable::rest(t, s)`, the same style
every other overflowing list in this app already uses), which turns the
overlap into an ordinary scroll instead of a paint-over. This is not a real
fix — the trail doesn't auto-scroll to the current (rightmost) crumb on
navigation, so a sufficiently deep path still needs a manual scroll to see
"where am I" without the fix. It only stops the visual collision.

**Likely fix shape**, either (or both, saola-theme's call):
- `widget::breadcrumb` takes an available-width budget (the same shape
  `overflow::unit_budget` already established) and collapses middle crumbs
  behind a single "…" pseudo-crumb, à la macOS Finder / GNOME Files — the
  crumb closest to the pattern this crate already ships for text.
- Or: keep the trail growing but auto-scroll to the trailing (current)
  crumb whenever the crumb list changes — needs an `Id` on the scrollable
  and a `scroll_to`-style operation the caller can trigger, which is more
  of a "give the consumer a hook" change than something saola-theme can
  fully own by itself.

## 3. `style::table::rest` is unconsumable on iced 0.14.2

**Status:** carried forward from Stage 12's handoff, unchanged. Still not
consumed anywhere in saola-files (no columns/detail view exists).

`Table` has no `.style()`/`.class()` builder in iced 0.14.2 — it's
hardwired to the default catalog in `Table::new`. A future columns/detail
view in this app (or any consumer) can only set
`.separator_x(0.0).separator_y(t.sizes.hairline)` and otherwise inherit the
default-catalog color, until a future iced release adds the missing
builder. Verified against the pinned tag's own `style/table.rs` doc
comment, which already documents this exact gap and the same workaround.

No action needed from saola-theme until iced itself grows the missing
builder — recorded here only so it isn't rediscovered as a saola-files bug
the day a columns view gets built.
