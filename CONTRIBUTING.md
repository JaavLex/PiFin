# Contributing to PiFin

Thanks for helping. PiFin is small on purpose. The one rule: **lightweight and fast**.

## The lightweight rule

PiFin runs on a Raspberry Pi 4 and starts in about 20 s. A change should not make that worse.
Before adding a dependency, a background process, a polling loop or a bigger image, ask if it
is worth it on a Pi. If a feature costs memory or CPU while idle, say so in the PR and
explain why it is worth it.

## Setup

See the Development section of the [README](README.md): build packages, Rust 1.92 or newer
from rustup, and `cd app && cargo build`. The Python services and tests need only Python 3
(and Node for one test).

## Tests

From the repository root unless noted:

```bash
cd app && cargo test                       # TV app unit tests
tools/selftest.sh                          # syntax checks, a throwaway card build, and the test scripts below
python3 tools/test_settingsd.py            # also test_remote_pairing.py, test_portal.py,
                                           # test_setup.py, test_updates.py
node tools/test_wizard.mjs                 # phone wizard logic
tools/check-leaks.sh                       # run after `git add`
```

Most of this runs on a PC. Playback, HDMI, CEC, Wi-Fi and the hotspot need a real Pi. If your
change touches those, test on a Pi and say which model and OS in the PR; if you can't, say
that instead and a maintainer will test it.

**A test has to prove something.** For every new test, break the code it covers (flip a
condition, delete a line) and check the test fails. Then undo the break. A test that passes
either way is not a test.

## Code style

- Rust: `cargo fmt` and `cargo clippy -- -D warnings` must be clean.
- Python and shell: match the surrounding code. Keep Python to the standard library.
- Comments say what the code does or why, in plain words. No internal task codes or ticket
  numbers ("T4", "D8") in code or comments.
- Keep functions and commits focused. One change per pull request.

## What never goes in git

- **No screenshots of your own library.** Use `tools/demo-server.py` (a fake Jellyfin) and
  `tools/shot-remote.mjs`.
- **No secrets and no Wi-Fi names, IP addresses or tailnet names.** Real values live in
  `pifin.env` and `secrets.env`, which are gitignored. Use placeholders such as
  `192.0.2.10` or `jellyfin.example` in docs and tests. `tools/check-leaks.sh` checks the
  staged tree; it also reads a local, gitignored `.leakwords` file with your own words to
  block.
- Redact logs and screenshots in issues too.

## Security issues

Do not open a public issue. Use GitHub's private vulnerability reporting: the **Security** tab
of the repository, then "Report a vulnerability". Say what you found, how to reproduce it and
which version.

## Pull request checklist

- [ ] One focused change, described in the PR.
- [ ] `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` pass (if `app/` changed).
- [ ] `tools/selftest.sh` passes.
- [ ] New tests fail when the code they cover is broken.
- [ ] `tools/check-leaks.sh` passes; no private data in code, docs or images.
- [ ] No new idle cost, or the cost is stated and justified.
- [ ] README or docs updated when behavior changed.
- [ ] Says what was tested on a Pi and what was not.
