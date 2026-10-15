# Changelog

## 0.1.2 (2026-10-15)

- Contractions survive: the apostrophe no longer ends a word, so `doesn't` is not cut down to `does't`
- Keys typed while a fix goes in come out in the order you typed them (`and` could come out as `dan`)
- Fixes keep their case when Caps Lock is on or Shift is still held from a `?`, `!` or `:`
- A typo of a common word is fixed instead of turned into a name or acronym: `fromm` becomes `from`, not `Fromm`, and `ne` is left alone instead of becoming `NE`
- Every fix is checked before it is typed: the result has to be a known word, and the start of the word has to have been seen, so a click or arrow key can no longer make a fix land on half a word
- Mouse clicks are noticed, so typing after a click no longer edits the wrong spot
- In apps that expose their text, each fix is read back once it lands. If it did not land as expected, or two fixes are undone in quick succession, auto-fix pauses in that window for a minute

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
