# CaseComplete — Research & Architecture

Date: 2026-10-14. Goal: a Windows-wide typing assistant. Ghost-text continuation you accept with Tab, plus fast typo/grammar repair, fully local, no UI beyond typing.

## 1. What exists

| Tool | OS | Approach | Model | Latency | Takeaway |
|---|---|---|---|---|---|
| Cotypist (closed) | macOS | Accessibility API, ghost text, Tab per word | Qwen2.5 1.5B / Gemma E2B | ~100 ms (TabType's measurement) | The product to match. Apple Silicon only. |
| TabType (MIT) | macOS | Listen-only key tap, shadow buffer, OCR/AX context, llama.cpp + prefix-cached KV, 9-wide beam over phrases, confidence gate, token healing, font-matched overlay | Qwen3-4B base / 1.7B | 148–217 ms | Best open design reference. Measured ~1/3 of next words are predictable. |
| KeyType (OSS) | macOS | AX snapshot, llama.cpp, constrained generation (trie logit masking), per-app overrides, pasteboard or keystroke insertion | Gemma 4 / Qwen3.5 | n/a | Constrained decoding idea worth borrowing later. |
| Fillit (MIT) | Win11 | Python/PyQt overlay positioned via UI Automation, hotkey-triggered | Groq cloud Llama 3 8B | ~300 ms | Proves a Windows UIA overlay works. Wrong backend, too slow. |
| SysInput (MIT, Zig) | Win | WH_KEYBOARD_LL hook, shadow buffer, trie dictionary, per-app insertion strategies | none | n/a | Native hook + injection reference. |
| EkaKey (C++) | Win | Hook + trie autocorrect + SendInput | none | n/a | Same. |
| Lightkey (closed) | Win | Offline n-gram style, up to 18-word predictions | proprietary | fast | The commercial incumbent on Windows. No LLM, no grammar. |
| Windows "text suggestions" (Settings > Typing) | Win | Word-level, app dependent | SwiftKey | fast | Too weak, not everywhere. |

Grammar engines:

| Engine | Speed | Notes |
|---|---|---|
| Harper (Rust, Apache-2) | ~10 ms | English only, in-process crate + WASM + LSP. Catches typos, agreement, common grammar. |
| LanguageTool (Java, LGPL) | ~650 ms | 30 languages, heavy (GBs RAM with n-grams). Too slow for keystroke path. |
| GECToR (tagging GEC) | ~10x faster than seq2seq | Edit-tag model, good for a "fix sentence" tier. Needs PyTorch/ONNX. |
| CoEdIT / T5 | slow | Seq2seq rewrite. Only for an on-demand hotkey. |
| Small instruct LLM | 30–80 ms per sentence on this GPU | "Fix only errors" prompt. Usable as tier 2. |

Finding: no open-source Windows tool combines system-wide ghost text + local LLM + grammar. We are building the Windows TabType, with a correction track added.

## 2. Measured on this machine

GPU: RTX 5060 Ti, 16 GB. Ollama `qwen2.5:1.5b`, 25-token prompt, 6 generated tokens, greedy:

| Run | total | prefill | gen |
|---|---|---|---|
| cold | 1733 ms | 104 ms | 39 ms |
| warm | 32 ms | 4 ms | 18 ms |
| warm | 33 ms | 4 ms | 19 ms |

≈3 ms/token warm. A 12-word suggestion costs ~60 ms of generation. A 150 ms keystroke-to-ghost budget is realistic.

## 3. Architecture

Single Rust binary (tray app) + bundled `llama-server` sidecar (CUDA build).

```
keystroke ─► [1 Hook thread] ─► shadow buffer ─► debounce 80–120 ms
                                                     │
                       ┌─────────────────────────────┴───────────────┐
              [2 Context]                                    [3b Correction]
     UIA TextPattern caret range                       Harper on last sentence (<10 ms)
     fallback: shadow buffer                           tier 2: small LLM "fix only", on pause
                       │                                             │
              [3a Completion]                                        │
     llama-server, base model, cached KV prefix,                     │
     n_predict ≤ 24, stop at sentence end, confidence gate,                     │
     token healing of partial word                                   │
                       └─────────────────┬───────────────────────────┘
                                  [4 Overlay] ghost text / underline chip
                                  [5 Inject] SendInput on Tab / word boundary
```

1. **Input.** `WH_KEYBOARD_LL` on its own thread; returns in <1 ms, never does work. Maintains a shadow buffer of the current field. Tab, Ctrl+Tab and Esc are swallowed only while a suggestion is visible; Left arrow and Backspace only inside the 3 s revert window; Ctrl+Alt+Z always. Skips password fields (UIA control type), excluded processes (terminals, password managers, games), and any field without focus.
2. **Context.** UIA `TextPattern`: zero-length range at the caret, expanded backwards ~800 chars. Fallback: shadow buffer only. Per-app override table (insertion strategy, context source, disabled). Chromium/Electron, Word, WinUI, WPF expose TextPattern; many Win32 and game windows do not.
3. **Engine.**
   a. Completion: `llama-server` single slot, `--cache-prompt`, base (non-instruct) model, raw prompt = recent writing + current field text. Generation is streamed, so the ghost shows the first words within ~60 ms and grows to the sentence end (≤24 tokens). Confidence gate on mean logprob, stricter mid-word. Token healing: strip the partial word, generate, keep only completions that start with it. Cancel in-flight request on next keystroke.
   b. Correction: `harper-core` in-process over the last sentence, plus a personal dictionary. Evaluated at word boundaries (space/punct/Enter) and auto-applied or suggested by severity (section 4). Tier 2 on ≥1 s pause: instruct model rewrites the last sentence, applied as a minimal diff under the same severity rules.
4. **Overlay.** Layered, topmost, no-activate, click-through window. Position from UIA caret bounding rect, fallback `GetGUIThreadInfo` caret, else hide. Grey ghost text in the field's font when UIA reports it. Corrections as a subtle underline chip.
5. **Injection.** `SendInput` unicode for accepted text. Corrections: backspace N + retype, or Shift+Ctrl+Left select + type, chosen per app. Clipboard paste with save/restore for long inserts (espanso pattern).
6. **Personalization.** Every change is logged (section 5). Opt-in corpus of your own writing (folders, exported emails) used as a "recent writing" prompt prefix now, and as LoRA training data later.

Why Rust, not Python: the hook thread and overlay need deterministic sub-millisecond behaviour; Fillit (Python) lands at ~300 ms. Why not a TSF IME: must be signed, complex, and apps still vary; every shipped product (Cotypist, TabType, Lightkey) uses hook + accessibility + overlay instead.

Models to bench (next-word top-1 on your own text, ms/token): Qwen2.5-1.5B base (have it), Qwen3-1.7B-Base, Gemma 3 1B, SmolLM2-360M, Gemma 3 270M (likely too weak). Use GGUF Q8 for the 1–2B class; VRAM is not a constraint.

## 4. Interaction model: continuous, severity-gated

The default mode is **continuous**: you just type. The engine listens to keystrokes and acts only when something changed and a decision point is reached. No polling, no timers except the completion debounce and the pause detector.

**Decision points ("when needed")**

| Trigger | What runs | Why |
|---|---|---|
| word boundary (space, punctuation, Enter) | correction check on the word just closed + last sentence | the word is final, safe to fix |
| 80–120 ms since last keystroke, caret at end of text | completion | user paused mid-thought |
| ≥1 s pause | tier-2 sentence check | deeper fix on a finished thought |
| cursor keys, mouse click, focus change | resync shadow buffer from UIA, hide ghost | nothing to fix, just re-anchor |
| field unchanged | nothing | no wasted inference |

**Severity levels.** Each candidate change gets a level. A single setting, `auto_apply_up_to`, decides which levels apply silently and which only suggest.

| Level | Class | Example | Default |
|---|---|---|---|
| S0 | Typo, one obvious fix, confidence ≥0.95 | `thhe→the`, `hahve→have`, `typiong→typing`, `usee→use` | auto-apply |
| S1 | Grammar/punctuation, one clear fix | `it allow→it allows`, missing apostrophe, double space | auto-apply |
| S2 | Ambiguous fix or multiple candidates | `there/their`, `its/it's` when context is weak | suggest (underline chip, Tab accepts) |
| S3 | Style / rewrite / tier-2 LLM sentence edit | wordiness, tense consistency | suggest, never auto |
| S4 | Content completion (rest of sentence) | ghost text | Tab accepts the whole predicted sentence; optional auto-commit for word-finish only |

Confidence comes from the source: Harper rule certainty + edit distance for S0/S1, LLM logprob for S3/S4. A word that you reverted twice goes into the personal dictionary and is never touched again. Code-like tokens (camelCase, paths, URLs, identifiers with digits) are exempt.

**Completion stays ghost by default.** Auto-inserting predicted *content* without a key would fight you when it is wrong, and about one in three next words is guessable. The only auto-commit allowed is `S4-word`: finishing the current partial word when the completion is a dictionary word with confidence ≥0.9. Off by default; opt-in once the log shows it is trusted. Everything longer waits for Tab. **Tab accepts the full predicted sentence** (up to the next sentence end, ≤ 24 tokens); Ctrl+Tab accepts one word for finer control; Esc dismisses. After a Tab the engine immediately predicts the next sentence, so Tab-Tab-Tab chains through a paragraph when the model has you right.

**Revert.** Left arrow, pressed as the very first key after an auto-applied change and within 3 s, reverts that change (original text restored, caret returned, the word added to a "don't touch this session" list). Any other key first, or after 3 s, left arrow is a normal cursor move. Backspace right after an auto-fix does the same. Ctrl+Alt+Z reverts the last auto-change at any time. The revert itself is logged as the strongest negative signal.

**Live visibility.** An auto-applied word gets a 1.5 s fade underline in the overlay so you see what moved. Off-able.

## 5. Profile, update log and learning loop

Everything personal lives in one **profile** (your "account"): `%LOCALAPPDATA%\CaseComplete\profiles\<name>\` holding `events.db`, `dictionary.txt`, `thresholds.json`, `style/` (recent-writing snippets), and `adapters/` (LoRA files). Profiles are plain folders: back up, sync, or switch with one tray click. Nothing leaves the machine.

Every decision is a row in the profile's `events.db`.

```
events(
  id, ts, app_exe, window_title_hash, field_kind,
  source        -- harper | dict | llm_fix | llm_complete
  rule_id       -- Harper lint name or prompt version
  severity      -- S0..S4
  confidence    -- 0..1
  context_before  -- up to 200 chars left of the span (opt-in; hashed if privacy mode)
  original_span, replacement_span,
  action        -- auto | suggested
  outcome       -- kept | reverted | accepted | ignored | overwritten | timeout
  outcome_ts, latency_ms
)
```

`kept` is set when 10 s pass or the sentence is submitted without the span being edited; `overwritten` when you edit the span yourself afterwards.

What the log feeds, in order of effort:

1. **Threshold tuning (automatic, nightly).** Per `rule_id`: revert rate >25 % over ≥20 events → demote one severity level (auto → suggest). Accept rate >90 % on a suggested rule over ≥50 events → offer promotion. Shown in the tray as a short list you can approve.
2. **Personal dictionary (automatic).** Any span reverted twice, or typed unchanged 5+ times across sessions, becomes a known word.
3. **Prompt context (automatic, continuous).** Last N accepted sentences, your phrasing of rejected ones (what you typed instead), and recent writing feed the prompt prefix. This is the fastest learning loop: it changes behaviour within the same session, no training step.
   Per-app style is tracked separately (email vs chat vs docs) so the prefix matches where you are typing.
4. **Fine-tuning (manual, phase 3).** Export pairs `(original_span, final_text)` and `(context_before, accepted_completion)` as JSONL. LoRA on the completion base model for your voice; a small GEC head or Harper rule overrides for corrections. A `train` command runs locally on the GPU and writes a new adapter the sidecar loads on restart.

Metrics shown in the tray: chars saved, auto-fixes kept vs reverted, suggestions accepted, p50/p95 keystroke-to-ghost.

## 6. Latency budget

| Stage | ms |
|---|---|
| debounce | 100 |
| context read (UIA) | 5 |
| prefill with cached prefix | 5–15 |
| first 3 tokens streamed | 10–15 |
| overlay paint | 5 |
| **keystroke → first ghost words** | **~140** |
| remaining tokens to sentence end (≤24) | +60–75, streamed in |

Correction path runs in parallel: Harper <10 ms.

## 7. Risks

- UIA coverage varies; shadow buffer must be good enough alone.
- Overlay placement with DPI scaling and multiple monitors.
- Antivirus heuristics on global hook + SendInput; sign the binary.
- Quality ceiling: ~1/3 of next words are guessable. Value comes from phrase completion and corrections, not predicting everything.
- Interference with apps' own autocomplete (VS Code, Outlook). Needs per-app disable.
- Left arrow as revert overloads a cursor key. Limited to the 3 s / first-key window to stay safe; if it still bites, swap to Backspace-only.
- Auto-apply in the wrong field (code editor, terminal, password) is the worst failure. Exclusion list ships conservative; code-token exemption is mandatory.

## 8. Phases

0. **Spike (1–2 days):** Rust hook + overlay + llama-server. Ghost text in Notepad, Chrome, VS Code chat, Outlook. Measure real keystroke-to-ghost.
1. **MVP:** continuous mode with S0/S1 auto-apply via Harper + dictionary, left-arrow/Backspace revert, event log, completion ghost + Tab, exclusions, tray icon, autostart.
2. **Quality:** severity setting UI, confidence gate, token healing, per-app overrides, recent-writing context, nightly threshold tuning, metrics.
3. **Personal:** corpus ingestion, JSONL export, LoRA on your writing, tier-2 LLM sentence fix on pause.

## Sources

- TabType: https://github.com/nilava/TabType
- KeyType: https://github.com/johnbean393/KeyType
- Cotypist background: https://www.implicator.ai/cotypist-the-german-indie-behind-ai-autocomplete-everywhere
- Fillit: https://github.com/ariobarin/Fillit
- SysInput: https://github.com/PeterM45/SysInput
- EkaKey: https://github.com/RanvirRox/EkaKey-autocorrect-globally
- Harper comparison: https://github.com/Automattic/harper/blob/master/COMPARISON.md
- GECToR: https://arxiv.org/abs/2005.12592
- Lightkey: https://www.lightkey.io/post/type-at-the-speed-of-mind-with-lightkey-ai-writing-assistant-interview-with-website-planet
- UIA TextPattern: https://learn.microsoft.com/en-gb/dotnet/framework/ui-automation/ui-automation-textpattern-overview
- Custom IME requirements: https://learn.microsoft.com/nb-no/windows/apps/develop/input/input-method-editor-requirements
- llama.cpp speculative decoding: https://www.mintlify.com/ggml-org/llama.cpp/inference/speculative-decoding
- Gemma 3 270M: https://www.theregister.com/2025/08/15/little_llm_on_the_ram/
