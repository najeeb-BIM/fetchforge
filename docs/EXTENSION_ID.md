# Stable extension ID — how it is derived

## The problem

An unpacked Chrome extension with no `key` in its manifest gets a
different ID on every machine and folder path. Native messaging only
allows the extension IDs listed in the host manifest's
`allowed_origins`, so a path-derived ID would break native messaging
after any move or reinstall.

## The fix

`extensions/chromium/manifest.json` now carries a fixed public `key`
(base64 DER of an RSA-2048 public key). Chrome derives the extension
ID from that key alone, not from the folder path, so the ID is stable
across machines, folders, and reinstalls.

- Public key: the `key` field in `extensions/chromium/manifest.json`
  (392 base64 characters, public only — it is not a secret).
- Extension ID: `hkmnehbdedolcaaikibkadblmbfkkija`
- Host manifest: `installer/host-manifest-chrome.json` lists exactly
  `chrome-extension://hkmnehbdedolcaaikibkadblmbfkkija/` in
  `allowed_origins`. If these two ever disagree, native messaging
  stops working — that is the check in `tests/token_hygiene.sh`.

## Derivation (reproducible)

1. Take the DER bytes of the public key (base64-decode the `key` field).
2. SHA-256 those bytes.
3. Take the first 16 bytes (128 bits).
4. Encode each nibble as a letter: 0→a, 1→b, …, 15→p.
   That gives a 32-letter ID using only a–p.

In Python:

```python
import base64, hashlib, json
key = json.load(open("extensions/chromium/manifest.json"))["key"]
der = base64.b64decode(key)
h = hashlib.sha256(der).digest()[:16]
ext_id = "".join(chr(ord("a") + ((b >> 4) & 15)) + chr(ord("a") + (b & 15)) for b in h)
# ext_id == "hkmnehbdedolcaaikibkadblmbfkkija"
```

Verify in Chrome: load the unpacked extension and check
`chrome://extensions` — its ID must be exactly the ID above.

## Private key

The RSA private key used to generate this public key was created in
`/tmp`, used only to extract the public key, and then shredded. It
was never copied into the repository, the source zip, or any backup.
Consequence, stated honestly: the public `key` alone is enough for a
stable unpacked ID, but a future Chrome Web Store submission would
need a new signing key (and a new store ID) because the original
private key was not retained. That is a deliberate trade-off for not
storing a private key in this project.

## Firefox

Firefox does not derive its ID from a key. It uses the fixed ID in
`browser_specific_settings.gecko.id` in
`extensions/firefox/manifest.json`, which is
`fetchforge@example.local`. That string is chosen by the developer,
not computed, and must match the ID in the Firefox host manifest's
`allowed_extensions` (`installer/host-manifest-firefox.json`).
Development only: this ID must be replaced with a proper permanent
ID before any public release. Changing it after users install will
break native messaging, because `allowed_extensions` must match.
