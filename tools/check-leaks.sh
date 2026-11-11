#!/usr/bin/env bash
# Gate before publishing: no secret value, no personal value, no local-only file in git.
# Run after 'git add': it checks the staged tree. Reports file names only.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)"
fail=0

# 1. Everything tracked must be on the publish list.
allowed='^(\.gitignore|LICENSE|README\.md|pifin\.env\.example|build\.py|files/|docs/|tools/)'
while IFS= read -r f; do
  if ! echo "$f" | grep -qE "$allowed"; then
    echo "NOT FOR PUBLISH: $f"; fail=1
  fi
done < <(git ls-files)
for local_only in CLAUDE.md plans/PROJECT.md secrets.env pifin.env; do
  git ls-files --error-unmatch "$local_only" >/dev/null 2>&1 && { echo "NOT FOR PUBLISH: $local_only"; fail=1; }
done

# 2. Private addresses, plus your own words from .leakwords (gitignored, one per
#    line: your username, Wi-Fi name, server name, timezone...). Never committed.
EXCL=(":!tools/check-leaks.sh")
if git grep --cached -q -I -E -e '\b(192\.168|10\.[0-9]+|100\.[0-9]+)\.[0-9]+\.[0-9]+\b' -- . "${EXCL[@]}" \
   ; then
  hits=$(git grep --cached -I -l -E -e '\b(192\.168|10\.[0-9]+|100\.[0-9]+)\.[0-9]+\.[0-9]+\b' -- . "${EXCL[@]}" | grep -v 'pifin-setup-screen.py' || true)
  if [ -n "$hits" ]; then echo "LEAK: a private IP address in:"; echo "$hits"; fail=1; fi
fi
if [ -r .leakwords ]; then
  while IFS= read -r w; do
    [ -z "$w" ] && continue
    if git grep --cached -q -I -F -w -i -e "$w" -- . "${EXCL[@]}"; then
      echo "LEAK: a word from .leakwords in:"; git grep --cached -I -l -F -w -i -e "$w" -- . "${EXCL[@]}"; fail=1
    fi
  done < .leakwords
else
  echo "note: no .leakwords file, only generic checks ran"
fi

# 3. The real secret values, without ever printing them.
tmp=$(mktemp); trap 'rm -f "$tmp"' EXIT
for f in secrets.env pifin.env; do
  [ -r "$f" ] || continue
  awk -F= '/^(export )?(PIFIN_PASS|PIFIN_PASS_HASH|TS_AUTHKEY|HOTSPOT_PASS|WIFI_SSID|WIFI_PSK|JELLYFIN_URL)=/ {
      v = substr($0, index($0, "=") + 1); gsub(/^[ \t]*['\''"]|['\''"][ \t]*$/, "", v);
      if (length(v) >= 5) print v }' "$f" >> "$tmp"
done
if [ -s "$tmp" ] && git grep --cached -q -I -F -f "$tmp"; then
  echo "LEAK: a secret value appears in:"; git grep --cached -I -l -F -f "$tmp"; fail=1
fi

# 4. Screenshots are binary: grep cannot see them, so say so out loud.
echo "REMINDER: eyeball docs/screenshots/*.png and *.jpg with your own eyes — grep is blind to pixels."

[ "$fail" -eq 0 ] && echo "no leaks" || exit 1
