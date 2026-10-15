# Contributing

Thanks for helping. A few things that keep the project easy to work on.

## Before you start

- Open an issue for anything bigger than a small fix, so we agree on the shape first.
- Windows 11, Rust stable 1.93+. `cargo build --release` builds everything; `cargo test --workspace --exclude cc-bench` runs the tests. Some tests are `#[ignore]` because they need a desktop session.

## Pull requests

- One behaviour per PR, with a subject line that states the observed change.
- Keep `cargo clippy --all-targets` and `cargo fmt` clean.
- A fix for a bug comes with a test that fails without it.
- Changes to the hook (`cc-hook`), injection (`cc-inject`) or the privacy gate (`cc-app/src/privacy.rs`) need a note on how you tested them in real apps; those are the parts that can hurt a user.

## What goes where

| Area | Crate |
|---|---|
| keystrokes, swallow rules | `cc-hook` |
| reading the caret and text | `cc-context` |
| model download, llama-server, completions | `cc-engine` |
| typos, grammar, autocorrect list, dictionary | `cc-correct` |
| ghost text, underline | `cc-overlay` |
| typing into the field | `cc-inject` |
| event log, metrics, tuning | `cc-store` |
| web UI and API | `cc-ui` |
| windows, tray | `cc-window`, `cc-tray` |
| orchestration, settings, privacy | `cc-app` |

## Models and licenses

Never commit model files. Adding a model means adding a `ModelSpec` in `cc-engine` with its Hugging Face repo, file name, size, and a license the README can state.
