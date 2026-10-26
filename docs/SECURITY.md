# Security

## Threat model

The attacker is a malicious website, a malicious link, or another local
user/process — not the user themselves. The bridge is a local service,
so its main risks are cross-site requests from a browser tab and a
malicious page tricking the extension into sending a dangerous URL.

## Controls, and where they live

- **Loopback only.** The bridge binds `127.0.0.1:18443` explicitly.
  It never binds `0.0.0.0`. A remote machine cannot reach it.
- **Host check (DNS-rebinding defence).** Even on loopback, a request
  is rejected (403) unless its HTTP Host is `127.0.0.1`, `localhost`,
  or `::1` (with or without a port). A browser tricked by DNS rebinding
  into contacting loopback via an attacker domain would send that
  domain as Host and is rejected.
- **Pairing, not token display.** Pairing codes are 8 CSPRNG
  characters from a 32-character no-look-alike alphabet, hashed
  with SHA-256 in memory only, 60-second expiry, single use, one
  active code at a time, invalidated after 5 wrong attempts
  (global counter, all clients are loopback). The token is
  returned only after a correct code, to the extension background
  script, which stores it in background storage only.
- **Challenge-response.** Before sending its stored token, the
  extension challenges the server with a random nonce and checks
  HMAC-SHA256(token, nonce). A server that does not know the
  token cannot pass. Token comparisons in the bridge use SHA-256
  hashes compared in constant time.
- **Rotation.** "Unpair all browsers" rotates the token in the
  token file and in memory and clears pairing state; old tokens
  return 401 immediately and the new token is not returned over
  the API.
- **Port binding.** If the bridge cannot bind 127.0.0.1:18443 it
  exits with a clear error and never picks another port silently.
- **Honest limit.** This protects against websites and web pages.
  It does NOT protect against malware already running as the same
  Windows user, which could read the token file or browser
  storage. See docs/TOKEN_PAIRING_DESIGN.md for the full threat
  model.
- **Per-install token.** `POST /downloads`, `GET /downloads`,
  pause/resume/cancel all require `X-FetchForge-Token`, a 32-hex-char
  secret generated on first run, stored with mode 0600 where the OS
  supports it, and never logged. `/health` is the only unauthenticated
  endpoint and returns only app name/version.
- **Origin check.** Browser `Origin` headers must be a
  `chrome-extension://` / `moz-extension://` origin or another loopback
  page. Other origins get 403.
- **Native messaging is the preferred path.** Only the extension ID
  registered in the host manifest can invoke the native host, so no
  token is involved on that path.
- **URL allowlist.** Only `http`/`https`, with a host, no embedded
  credentials, max 8 KiB. `file:`, `javascript:`, `data:`, `ftp:` are
  rejected by the engine, so the extension cannot be used to read local
  files or run scripts.
- **Filename safety.** Filenames are stripped to a single segment,
  forbidden Windows characters become `_`, trailing dots/spaces are
  removed, reserved names (`CON`, `NUL`, `COM1`…) are prefixed, and the
  result is joined under the download directory only. No path
  traversal is possible through a filename or a redirect.
- **No execution.** Downloaded files are never executed, opened, or
  launched by the app. The user opens them themselves.
- **No credential harvesting.** The extension does not read cookies,
  passwords, or history. Its permissions are contextMenus, downloads
  (fallback only), and storage (token). It never sends browsing data
  anywhere; the bridge never contacts a third-party server.
- **No DRM bypass.** HLS with `EXT-X-KEY` and DASH with
  `ContentProtection` are refused with a plain explanation. No
  paywall, sign-in, or access control is bypassed.
- **Local data only.** History and settings live in
  `%LOCALAPPDATA%\FetchForge` (Windows) / `~/.local/share/fetchforge`
  (Linux dev). Nothing is uploaded.
- **Logs.** The bridge logs bind address and data dir only — never
  URLs with query strings, tokens, or filenames beyond what the user
  sees in their own dashboard.

## What is NOT protected against

- A user who pastes their bridge token into a website, or installs a
  malicious extension with the same token, has given that party local
  download control. Treat the token like a password: it is shown only
  in the app's Settings, never in a web page.
- The loopback token does not protect against another local user
  account on the same machine reading the token file if file
  permissions are weakened. Per-user install + 0600 token file is the
  mitigation; shared machines should use separate Windows accounts.
- TLS is terminated by the origin server (reqwest/rustls). A download
  over plain `http` is not encrypted; the UI shows the URL so the user
  can see that.
