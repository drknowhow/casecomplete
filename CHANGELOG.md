# Changelog

## 0.1.1 (2026-10-15)

- A startup error is logged and shown in a message box instead of the app quitting without a word
- If the first-run download fails, the app stays in the tray with completions off and says why; corrections keep working
- Downloads trust the Windows certificate store and use the Windows proxy settings, so they work behind corporate TLS inspection

## 0.1.0 (2026-10-14)

First public build.

- Typo and grammar auto-fix at the word boundary (Harper plus a seeded autocorrect list), blue underline, undo with Left arrow or Backspace within 3 s, Ctrl+Alt+Z any time
- Sentence ghost text with confidence gate and token healing; Tab accepts and chains, Ctrl+Tab one word
- Learning: retyped words become autocorrect entries, double undo adds to the dictionary, rules demote after too many reverts
- Privacy gate for card numbers, SSNs, IBANs, long digit runs, labelled secrets; app and window-title denylist
- Models: Gemma 3 1B (default), SmolLM2 360M, Qwen2.5 1.5B; switch at runtime
- Tray flyout and insights dashboard (WebView2), event log, per-app stats, context panel
- Per-user MSI installer, start with Windows
