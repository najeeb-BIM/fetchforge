# Token UX redesign — APPROVED and IMPLEMENTED (engine + bridge, Linux only)

Status, updated 2026-10-26: the design below was approved with
conditions and implemented in `crates/engine/src/pairing.rs` and
`crates/bridge/src/main.rs`, tested on Linux only (see
`tests/pairing_e2e.sh` and `tests/token_hygiene.sh`). Extension popup
and background changes for pairing and challenge-response are written
in this round but are NOT VERIFIED until tested in a real browser.
Manual token entry stays behind the Advanced toggle with the red
warning "Never paste this into a website" and will be removed once
pairing is verified in a real browser.

Approved conditions, as implemented:
1. Stable extension ID: Chromium manifest carries a fixed public
   `key`; the derived ID is `hkmnehbdedolcaaikibkadblmbfkkija`
   (see docs/EXTENSION_ID.md). Firefox uses the fixed gecko ID
   `fetchforge@example.local`. The private key was shredded and
   never stored in the repository.
2. Pairing code: OS CSPRNG (`getrandom`), 8 characters from the
   32-character alphabet `ABCDEFGHJKLMNPQRSTUVWXYZ23456789` (no
   0/O, 1/I/L), SHA-256 hash stored in memory only, constant-time
   hash compare (no early return), 60-second expiry, single use,
   one active code at a time. After 5 wrong attempts the active
   code is INVALIDATED (this replaces the earlier 60-second
   lockout idea — an invalidated code can never be used, even
   with the correct code). Lockout counting is global because
   the state is a single shared PairingState and all clients are
   loopback.
3. Challenge-response: before the extension sends its stored token,
   it sends a random nonce to `POST /challenge`; the bridge
   answers with HMAC-SHA256(token, nonce); the extension verifies
   it locally and refuses to send the token on mismatch. If the
   bridge cannot bind 127.0.0.1:18443 it exits with a clear error
   ("could not bind … Is another copy already running?") and
   never picks another port silently.
4. Token hygiene: the token lives only in extension background
   storage (`storage.local`, read only in background.js). No
   content script exists, the token is never in a page context,
   URL, or log, and the popup never shows the stored token.
   `tests/token_hygiene.sh` greps for all of this.
5. "Unpair all browsers": `POST /unpair-all` (token required)
   rotates the token in the token file and in memory, clears
   any active pairing code, and old tokens return 401 immediately.
   The new token is NOT returned by the endpoint; the app reads
   it from the token file and browsers must re-pair.
6. Manual token entry: behind the Advanced toggle, red warning
   "Never paste this into a website", removed once pairing is
   verified in a real browser (see above).
7. Threat model: see below, stated honestly.
8. Linux-only proof: code generation, hashing, expiry, single
   use, lockout/invalidation, challenge-response, and rotation
   are all tested on Linux. Extension UI is written, not verified.
9. Still not built: scheduling, notifications, DASH,
   retry/remove/sort.

## Threat model — stated honestly

This design protects against websites and web pages:
- A malicious web page cannot call the bridge directly because of
  loopback binding, Host check, Origin check, and the token.
- A malicious web page cannot steal the token because there is no
  content script, the token is never in a page context, URL, or
  log, and the popup never displays it.
- A fake server that does not know the token cannot pass the
  challenge-response check, so the extension will not send the
  token to it — provided the extension already holds a real token
  from a real pairing.

It does NOT protect against malware already running as the same
Windows user. Such malware could read the token file from the
user's data directory, read the browser profile's extension
storage, bind the loopback port before FetchForge starts, or
simply run code as that user. No loopback token, pairing code,
or HMAC can stop code that already runs with the user's own
permissions. The pairing code's 60-second window and single use
limit shoulder-surfing and accidental exposure, not local malware.

## The problem today

To pair a browser, the user must open FetchForge Settings, copy a
32-character hex token, open the extension popup, and paste it exactly.
That is error-prone for a non-developer, and a long-lived secret shown
on screen invites copying it into the wrong place.

## Recommended design: native messaging first, 60-second pairing second

### Path 1 — Native messaging (preferred, already in code, unproven in a browser)

- The Windows installer registers the bridge as a native-messaging host
  for Chrome/Edge/Firefox, listing only FetchForge's extension ID.
- The extension calls the native host directly. The browser itself
  authenticates the caller by extension ID, so no token is typed,
  shown, or stored by the user at all.
- Popup shows "Connected" with no setup step.
- Limit: only works when the installer registered the host. Portable
  installs, moved installs, and browsers installed after FetchForge
  fall through to Path 2.

### Path 2 — One-time pairing code, 60 seconds, single use (fallback)

1. In FetchForge, the user clicks Settings > "Pair a browser".
   The app generates a random 8-character code, shown as two groups
   (for example `7K3Q-9M2P`), and starts a 60-second countdown.
2. Only a SHA-256 hash of the code is kept, in memory only, never
   written to disk and never logged. The code itself is shown only
   in the app window.
3. In the extension popup, the user clicks "Pair" and types the code.
   The extension sends it to the loopback bridge (`POST /pair`).
4. The bridge checks, in order: Host is loopback, Origin is an
   extension origin, the code hash matches, it has not expired
   (60 seconds), and it has not been used before. At most 5 wrong
   attempts are allowed; after the 5th wrong attempt the code is
   invalidated (see the approved conditions above — this replaced
   the earlier 60-second lockout idea).
5. On success, the bridge marks that extension instance as paired and
   the extension stores the existing bridge token internally in
   `storage.local`. The user never sees, types, or copies the token.
   The pairing code is deleted immediately after one successful use.
6. On expiry or failure, the popup says "Code expired — generate a
   new one in FetchForge" and nothing is paired.

### Revocation and the manual token

- v1 keeps a single bridge token (as today), so "Unpair all browsers"
  in Settings regenerates that token and forces every browser to
  re-pair. Per-browser tokens are a later improvement, not in v1.
- The manual token display is kept behind the Advanced toggle with
  the red warning "Never paste this into a website" for one
  release after pairing ships, then removed once pairing is
  proven in a real browser. Removing it on day one would leave
  portable users with no fallback if pairing has a bug.

## Security notes

- A 60-second, single-use, 8-character code (62^8 combinations) with
  a 5-attempt lockout cannot realistically be guessed in its lifetime.
- Shoulder-surfing is limited by the 60-second window, single use,
  and the fact that the code only works from loopback with a valid
  extension Origin and Host.
- Pairing must be started by the user clicking in the app; there is
  no remote or automatic pairing, and no code is ever sent over the
  network — everything stays on loopback.
- After pairing, the existing protections stay in place: loopback
  only, Host check, Origin check, and token required for every
  download/list/pause/resume call.

## What approval means

Approving this design authorises implementation only — bridge `/pair`
endpoint, code generation/hash/expiry/rate-limit, app Pair button with
countdown, and extension Pair field — followed by the same proof
process (unit tests, failure tests, real-browser test) before the
manual token display is removed. It does not authorise scheduling,
notifications, DASH, or retry/remove/sort, which stay blocked until
the Windows build and real-browser tests pass.

## Decision needed from the user

- Approve the recommended design (native first + 60-second pairing fallback), or
- Approve pairing only (skip native messaging), or
- Keep manual copy-paste for now.
