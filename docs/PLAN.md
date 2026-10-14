# CaseComplete — Implementation Plan

Companion to `research-and-architecture.md`. That file says *what* and *why*; this one says *how*, in what order, and how we know each step is done.

## 0. Stack

| Part | Choice | Reason |
|---|---|---|
| Language | Rust (stable), Cargo workspace | sub-ms hook thread, single signed binary, `harper-core` is Rust |
| Win32 | `windows` crate (`Win32_UI_Input_KeyboardAndMouse`, `Win32_UI_Accessibility`, `Win32_Graphics_Direct2D`, `Win32_Graphics_DirectWrite`) | official bindings, no C glue |
| Grammar | `harper-core` | ~10 ms, in-process |
| LLM | bundled `llama-server` (CUDA build) as child process, HTTP on localhost, streaming `/completion` | prefix KV cache, swap models without rebuild |
| Store | `rusqlite` (bundled SQLite) | zero-install, single file per profile |
| Tray / config | `tray-icon` + `muda` menus, TOML config via `serde` | no GUI framework needed |
| Async | `tokio` (current-thread runtime off the hook thread) | cancellation of in-flight completions |
| Training | Python 3.14 + `peft`/`unsloth` in `tools/train/` | runs only on demand |
| Tests | `cargo test` + a UIA-driven Notepad harness | real end-to-end text checks |

## 1. Repository layout

```
CaseComplete/
  Cargo.toml                 workspace
  crates/
    cc-hook/       WH_KEYBOARD_LL thread, shadow buffer, key classifier, revert window
    cc-context/    UIA TextPattern read, caret rect, focused app/field info, per-app overrides
    cc-engine/     completion client (llama-server), correction (harper + dictionary), severity, confidence, token healing
    cc-overlay/    layered click-through window, Direct2D/DirectWrite ghost + underline chip
    cc-inject/     SendInput strategies, clipboard fallback, undo plan builder
    cc-store/      profile folder, events.db schema + migrations, dictionary, thresholds, metrics
    cc-app/        main.rs: tray, config, orchestrator state machine, sidecar manager, nightly job
  tools/
    bench/         Rust bin: next-word top-1 and ms/token on a text corpus against llama-server
    train/         Python: JSONL export → LoRA → GGUF adapter
    e2e/           UIA harness that types into Notepad/Chrome and asserts final text
  models/          GGUFs (gitignored), download script
  bin/             llama-server.exe + CUDA DLLs (gitignored, fetched by script)
  docs/
```

## 2. Core runtime: orchestrator state machine

Lives in `cc-app`. One task, fed by a channel from the hook thread.

```
Idle ──key──► Typing(buf)
Typing ──word boundary──► Correcting ──(auto S0/S1)──► Injecting ──► Typing
                                      ──(suggest S2/S3)──► ShowingChip ──Tab──► Injecting
Typing ──100 ms quiet, caret at end──► Completing(req) ──first tokens──► ShowingGhost
ShowingGhost ──Tab──► Injecting(sentence) ──► Completing(next)
ShowingGhost ──Ctrl+Tab──► Injecting(word) ──► Completing
ShowingGhost ──typed char matches ghost──► ShowingGhost (advance)   ──mismatch/Esc──► Typing (cancel req)
Injecting(auto) ──Left/Backspace in 3 s, or Ctrl+Alt+Z──► Reverting ──► Typing
any ──focus/caret jump/mouse──► Resync (UIA read) ──► Idle
any ──1 s quiet──► Tier2Check (optional) ──► ShowingChip | Typing
```

Hard rules:
- The hook callback only classifies the key, updates the shadow buffer, decides swallow/pass, and posts an event. Never blocks, never calls UIA or HTTP.
- Every inference request carries a generation id; results for a stale id are dropped.
- Injection is serialized; nothing else writes to the field while an injection is in flight. Keys typed during injection are queued and replayed.

## 3. Milestones

### M0 — Spike (days 1–2)
Goal: prove ghost text end to end on this machine and get real numbers.
- [ ] Workspace, `cc-hook` with `WH_KEYBOARD_LL` on its own thread, shadow buffer, Tab/Esc swallow when ghost visible
- [ ] `cc-overlay`: layered topmost no-activate window, draw grey text at a given point
- [ ] Caret position via UIA `TextPattern` bounding rect, fallback `GetGUIThreadInfo`
- [ ] `cc-engine`: start `llama-server` with `qwen2.5-1.5b` base GGUF, `--cache-prompt`, streaming completion, cancel on keystroke
- [ ] `cc-inject`: `SendInput` unicode for accepted text
- [ ] Timing log: keystroke → first ghost token, per app
- Accept: ghost appears and Tab inserts correctly in Notepad, Chrome (Gmail compose), Outlook, VS Code chat; p50 keystroke → first ghost < 200 ms in Notepad.

### M1 — Completion loop done right (week 1)
- [ ] `cc-context`: read ~800 chars before caret via UIA; shadow-buffer fallback; detect password/readonly fields; focused process name
- [ ] Exclusion list (terminals, password managers, games) + "disabled in this app" tray toggle
- [ ] Sentence-end stop, `n_predict ≤ 24`, token healing of the partial word, confidence gate (mean logprob; stricter mid-word)
- [ ] Ghost advances as you type matching characters; mismatch cancels
- [ ] Ctrl+Tab accepts one word; Tab accepts sentence then immediately requests the next
- [ ] Resync on caret jump / mouse / focus change
- [ ] Tray: on/off, pause 10 min, current app status
- Accept: 30-minute real use in email and chat with no wrong insertions and no swallowed keys when nothing is shown.

### M2 — Corrections and revert (week 2)
- [ ] `harper-core` lint over the last sentence at word boundary; map lints → candidate changes
- [ ] Personal dictionary (profile `dictionary.txt`) + code-token exemption (camelCase, paths, URLs, digits)
- [ ] Severity classifier S0–S3 with `auto_apply_up_to` setting (default S1)
- [ ] Auto-apply via `cc-inject` undo plan (backspace N + retype, or select-back + type, per app); verify via UIA re-read where available
- [ ] Revert: Left arrow / Backspace as first key within 3 s; Ctrl+Alt+Z anytime; reverted span → session don't-touch list
- [ ] 1.5 s fade underline on auto-applied word; chip + Tab for S2/S3
- Accept: the sentence in the first message of this project (`thhe`, `hahve`, `lleft`, `usee`, `typiong`, `hoow`) is fixed live while typing in Notepad and Gmail; each fix reverts cleanly with Left arrow; no fix is attempted inside a code block in VS Code.

### M3 — Profile, log, learning (week 3)
- [ ] Profile folder layout, create/switch in tray
- [ ] `events.db` schema + migrations; every decision logged with outcome (`kept` after 10 s / submit, `overwritten` on later edit)
- [ ] Metrics in tray: chars saved, auto-fixes kept vs reverted, p50/p95 latency
- [ ] Style prefix: last N accepted sentences + what you typed instead of rejected ones, per app class (mail / chat / docs)
- [ ] Nightly job: per-rule demotion (>25 % reverts over ≥20) and promotion offers (>90 % accept over ≥50); dictionary auto-add
- Accept: after a week of use the tray shows non-zero metrics, at least one rule has been auto-demoted or a word auto-added, and the style prefix visibly changes suggestions.

### M4 — Model quality and ship (week 4)
- [ ] `tools/bench`: next-word top-1 and ms/token on an exported corpus of your writing; run Qwen2.5-1.5B base, Qwen3-1.7B-Base, Gemma 3 1B, SmolLM2-360M
- [ ] Pick default model; download script with checksums
- [ ] Per-app overrides table (`apps.toml`): context source, insert strategy, disabled, font
- [ ] Tier-2 sentence fix on 1 s pause with a small instruct model, minimal diff, S3 only
- [ ] Installer (MSI via `cargo-wix`), autostart, code signing, crash log
- [ ] `tools/e2e`: UIA-driven Notepad harness in CI (GitHub Actions `windows-latest`, no GPU: mock engine)
- Accept: clean install on a second Windows 11 machine works from the MSI with no manual steps.

### M5 — Training (after M4)
- [ ] JSONL export: `(context_before, accepted_sentence)` and `(original_span, final_text)`
- [ ] `tools/train`: LoRA on the completion base model (unsloth), convert to GGUF adapter, `llama-server --lora`
- [ ] Harper rule overrides from correction pairs
- Accept: bench top-1 on held-out personal text improves over the base model.

## 4. Testing

| Layer | How |
|---|---|
| shadow buffer, key classifier, revert window | unit tests with scripted key sequences |
| severity classifier, token healing, confidence gate | unit tests on fixtures (the typo sentence above is fixture #1) |
| inject undo plans | property tests: apply then revert equals original |
| UIA context, overlay placement | `tools/e2e` against Notepad and Chrome, asserts final field text via UIA |
| latency | timing log, p50/p95 asserted in e2e on this machine |
| store | migration tests, outcome transitions |

## 5. Config (`%LOCALAPPDATA%\CaseComplete\config.toml`)

```toml
profile = "dimitri"
auto_apply_up_to = "S1"      # S0 | S1 | S2 | off
word_finish_autocommit = false
debounce_ms = 100
pause_ms = 1000
revert_window_ms = 3000
model = "qwen2.5-1.5b-base-q8"
tier2_model = ""             # empty = off
show_fade_underline = true

[exclude]
exe = ["WindowsTerminal.exe", "1Password.exe", "KeePassXC.exe"]
```

## 6. Order of work this week

1. M0 in `cc-hook`, `cc-overlay`, `cc-engine`, `cc-inject` with the thinnest possible `cc-app`.
2. Measure. If first-ghost p50 is over 200 ms in Notepad, fix that before anything else (prefix cache, prompt size, model).
3. Then M1.

## 7. Model availability on another machine

The app does not depend on Ollama or anything pre-installed.

- First run downloads its own GGUF (~1.5 GB, checksummed) into `%LOCALAPPDATA%\CaseComplete\models\` and ships `llama-server` with CUDA, Vulkan and CPU builds; it picks the fastest one that works.
- No GPU: falls back to a smaller model (SmolLM2-360M or Gemma 3 270M class) on CPU. Slower but usable. Corrections via Harper need no model at all and work immediately.
- Until the download finishes, corrections run and completion is simply off; the tray shows progress.
- The profile folder is separate from the model, so copying it to a new machine carries dictionary, thresholds, log and adapters.

## 8. Open decisions

- Default model after bench (lean: Qwen2.5-1.5B base, already on disk via Ollama; we need the raw GGUF for llama-server).
- Whether `S4-word` auto-commit ever ships on by default. Decide from the log at M3.
- Left arrow as revert: keep unless it bites in week 2, then Backspace only.
