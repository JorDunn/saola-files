# Upstream saola-theme debt

Consolidated at the end of saola-files' Stage 16 (polish + release), for
whoever picks up saola-theme work next. Each item below is something
saola-files' CLAUDE.md forbids fixing locally ("zero hardcoded colors or
sizes… if a style is missing, add it to saola-theme — never restyle
locally"), so it's parked here instead of patched around in this repo.

None of these block saola-files v0.1.0 shipping — they're recorded so the
next saola-theme release can pick them up deliberately, not rediscover them
as regressions.

## 1. `style::table::rest` is unconsumable on iced 0.14.2

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
