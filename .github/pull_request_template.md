<!--
The PR title is the Conventional Commit, e.g. `feat(ui): add the filter bar`.
It becomes the squash commit and the changelog line; CI checks it.
A `BREAKING CHANGE:` footer goes at the end of this body.
-->

Closes #

## What and why

## Validation

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all-features`
- [ ] `cargo build --all-features`
- [ ] `cargo build --no-default-features`

## Live check (run by the orchestrator after merge)

<!-- Steps to verify in the running app. Worded results only; screenshots stay local. -->

## Notes for the next task
