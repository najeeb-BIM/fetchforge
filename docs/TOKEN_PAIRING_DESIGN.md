# Token UX redesign — DESIGN ONLY, awaiting user approval

Status: NOT implemented. No code in this round changes how tokens work.
The manual copy-paste token path in the app and extension is unchanged
until the user approves a design below and it is tested in a real browser.

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
   attempts are allowed; after that, pairing locks for 60 seconds.
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
- The manual token display is kept as a hidden advanced fallback for
  one release after pairing ships, then removed once pairing is
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
