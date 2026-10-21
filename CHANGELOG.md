# Changelog

## 0.1.4 (2026-10-20)

- The language model is asked about each word as you start typing it, so a fix that needs its view rarely holds back your keys
- A misspelling with several possible fixes is fixed when the model clearly expects one of them in the sentence and the keyboard slip explains it; otherwise it is left as you typed it. These fixes are tracked as their own rule, `spell_context`, so undoing them demotes only them
- A doubled-key fix can take the spelling the model expects instead, when the model's answer is already in
- When the model is slow to answer (for example while another program keeps the GPU busy), fixes that need it are skipped rather than guessed

## 0.1.3 (2026-10-20)

- Auto-fix no longer joins or splits words. An edit across two words (`out side`, `every one`, `had had`) is never applied automatically, and a fix replaces one word with one word unless it comes from your autocorrect list
- A fix starts only when no key you typed is still on its way to the app and, in apps that expose their text, only after reading that the field ends with the word it replaces
- Spelling is auto-fixed only for doubled keys (`thhe`, `soo`) or a single slip that keeps the first letter in a word of four letters or more (`documnet`). Less certain guesses such as `calory` or `morth` are left as you typed them, and so are abbreviations and coined words (`xlsx`, `htmlr`)
- A guessed fix, one that is not doubled keys or on your autocorrect list, first asks the language model, and is skipped when the model expects another word or gives no answer within 300 ms
- Auto-fix works only where completions do: editable fields that are not passwords, in apps the privacy settings allow. It used to type into read-only pages, password fields and apps turned off in privacy settings, and could learn typos there
- The Bold level adds only doubled letters in capitalised words (`Outllook`); its other guesses were kept about a third of the time
- A fix you undid and never kept is not typed again, overwriting a fix within 3 s counts as undoing it, and rules demoted for too many undos stay demoted after a restart
- When more than 1 in 50 of an app's recent fixes land wrong, auto-fix turns off in that app and the dashboard says so
- Autocorrect entries are learned without the punctuation around the typo

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
