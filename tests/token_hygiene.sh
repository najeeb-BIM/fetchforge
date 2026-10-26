#!/usr/bin/env bash
# Token hygiene + stable extension ID checks (static, no browser needed).
# Fails if any check fails. The token must live only in background storage.
set -u
cd "$(dirname "$0")/.."
fail() { echo "FAIL: $1"; exit 1; }
ok() { echo "PASS: $1"; }

# 1. No content script in either manifest (token could never leak to a page that way).
for m in extensions/chromium/manifest.json extensions/firefox/manifest.json; do
  grep -q "content_scripts" "$m" && fail "content_scripts found in $m"
done
ok "no content_scripts in either manifest"

# 2. Only background.js may set the X-FetchForge-Token header.
for f in extensions/chromium/popup.js extensions/firefox/popup.js; do
  grep -q "X-FetchForge-Token" "$f" && fail "token header set in $f (must be background only)"
done
grep -q "X-FetchForge-Token" extensions/chromium/background.js || fail "token header missing in background.js"
ok "token header only in background.js"

# 3. Popup must never preload the stored token into its input, and must clear it after save.
for f in extensions/chromium/popup.js extensions/firefox/popup.js; do
  grep -q 'getElementById("token").value = s.token' "$f" && fail "popup preloads token into DOM in $f"
  grep -q 'input.value = ""' "$f" || fail "popup does not clear token input after save in $f"
done
ok "popup never shows the stored token"

# 4. No token in any console.log, URL, or cookie in extension code.
for f in extensions/chromium/background.js extensions/chromium/popup.js extensions/firefox/background.js extensions/firefox/popup.js; do
  if grep -n "console\.log" "$f" | grep -qi "token"; then fail "token in console.log in $f"; fi
  if grep -n "token" "$f" | grep -qi "location\|href\|cookie\|search"; then fail "token near URL/cookie in $f"; fi
done
ok "no token in logs, URLs, or cookies in extension code"

# 5. Advanced toggle carries the exact red warning.
for f in extensions/chromium/popup.html extensions/firefox/popup.html; do
  grep -q "Never paste this into a website" "$f" || fail "red warning missing in $f"
  grep -q 'id="advanced"' "$f" || fail "Advanced toggle missing in $f"
  grep -q 'id="pairCode"' "$f" || fail "pairing code input missing in $f"
done
ok "Advanced toggle + red warning + pairing input present"

# 6. Stable Chrome ID: derive it from the manifest key and compare with the host manifest.
ID=$(python3 - <<'PY'
import base64, hashlib, json
key = json.load(open("extensions/chromium/manifest.json"))["key"]
der = base64.b64decode(key)
h = hashlib.sha256(der).digest()[:16]
print("".join(chr(ord("a") + ((b >> 4) & 15)) + chr(ord("a") + (b & 15)) for b in h))
PY
)
[ "$ID" = "hkmnehbdedolcaaikibkadblmbfkkija" ] || fail "derived Chrome ID is $ID, expected hkmnehbdedolcaaikibkadblmbfkkija"
grep -q "chrome-extension://$ID/" installer/host-manifest-chrome.json || fail "host manifest allowed_origins does not match derived ID $ID"
ok "Chrome ID $ID matches host manifest"

# 7. Firefox gecko ID is fixed and matches the host manifest.
GECKO=$(python3 -c "import json; print(json.load(open('extensions/firefox/manifest.json'))['browser_specific_settings']['gecko']['id'])")
[ "$GECKO" = "fetchforge@example.local" ] || fail "Firefox gecko id is $GECKO"
grep -q "$GECKO" installer/host-manifest-firefox.json || fail "Firefox host manifest does not list $GECKO"
ok "Firefox gecko id $GECKO matches host manifest"

# 8. No private key, token file, or .env in the tracked tree.
if grep -rn "BEGIN.*PRIVATE KEY" --exclude-dir=target --exclude-dir=.git --exclude=token_hygiene.sh . > /dev/null; then fail "private key found in tree"; fi
if git ls-files | grep -Eq "bridge-token\.txt|\.env$|\.pem$|\.key$"; then fail "token/key/env file is tracked"; fi
ok "no private key, token file, or .env tracked"

echo "TOKEN HYGIENE DONE"
