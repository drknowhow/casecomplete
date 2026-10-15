# Security

CaseComplete runs a keyboard hook and injects text, so it deserves a careful look.

## Scope

- Everything runs locally. The only network calls are the first-run downloads of llama.cpp from GitHub releases and the model from Hugging Face, both verified by SHA-256.
- The web UI binds `127.0.0.1` on a random port with a per-run token; requests without the token, or with a foreign `Host`/`Origin`, are refused.
- The privacy gate blocks completions, fixes and logging for text that looks like a card number, SSN, IBAN, long account number, or a labelled secret, and for apps and window titles on the denylist.

## Reporting

If you find a way to read what someone types, exfiltrate text, inject into a field they did not intend, or bypass the privacy gate, please report it privately: open a [security advisory](https://github.com/drknowhow/casecomplete/security/advisories/new) on GitHub. Expect an acknowledgement within a few days.

Please do not open a public issue for security problems.
