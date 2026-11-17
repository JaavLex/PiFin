## What and why

## Tested

On a PC:
On a Pi (model, OS), or "not tested on a Pi":

## Checklist

- [ ] `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test` pass (if `app/` changed)
- [ ] `tools/selftest.sh` passes
- [ ] New tests fail when the code they cover is broken
- [ ] `tools/check-leaks.sh` passes; no secrets, Wi-Fi names, IPs or screenshots of a real library
- [ ] No new idle CPU, memory or boot cost, or the cost is stated above
- [ ] README or docs updated if behavior changed
