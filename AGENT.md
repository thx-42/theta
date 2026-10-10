# AGENT.md

Instructions for AI agents working in this repo.

## Default: push every change to `dev`

- Work on branch `dev`. Never commit directly to `main`.
- After each modification: commit and push to `origin/dev`.
- Do not merge to `main` unless the user asks.

## When the user asks for a release / merge to `main`

1. Bump `version` in `Cargo.toml` (and `Cargo.lock`). Patch for fixes, minor for features, major for breaking changes.
2. Update `CHANGELOG.md`.
3. Merge `dev` into `main` via a full pull request (summary, features, fixes, test notes).
4. Stable release notes are written by hand: explain changes and new features. Do not use `--generate-notes`.
5. Return to `dev` afterwards.
