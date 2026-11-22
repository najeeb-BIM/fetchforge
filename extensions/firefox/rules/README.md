# FetchForge ad blocking (declarativeNetRequest)

Wherever the FetchForge extension is active, ad/popunder blocking is
active too (user requirement, 2026-11-22). Streaming sites bury their
players under popunders, casino/betting popups and fake "premium"
widgets; the blocker's job is to keep those off the page so the
download button is clickable and the player is usable.

## How it works

- `ads.json` is a static MV3 ruleset, enabled by default in the
  manifest (`declarative_net_request.rule_resources`, id `ff-ads`).
  Static rulesets need no `<all_urls>` host permission: the
  `declarativeNetRequest` permission alone lets the rules match.
- Rule 1 (priority 100) is a safety net: any URL ending in a media
  extension (.m3u8, .mpd, .mp4, .webm, .m4s, .m4v, .mov, .mkv, .ts)
  is explicitly ALLOWED, so a block rule can never break a download
  or a stream — even if the media sits on a listed domain. An ad
  video playing is cosmetic; a broken download is not.
- Rules 10+ block one curated ad/popunder/tracker domain each
  (`||domain^` covers subdomains). Resource types cover pages,
  frames, scripts, XHR, images, styles, fonts, objects, websockets,
  pings and `other` — deliberately NOT `media`.
- The popup has an on/off checkbox (default ON), persisted in
  `storage.local` as `adBlockEnabled`; the background script
  re-applies the stored choice at startup.

## What it does NOT block (known limits)

- Rotating throwaway casino/betting domains: they change faster than
  any list. The networks that serve them are blocked, which stops
  most popunders, but a brand-new domain can slip through until it
  is added here.
- Same-origin fake widgets (a "premium activated" box baked into the
  page's own HTML/JS): DNR only blocks network requests, not DOM.
- Extensionless stream URLs cannot be matched by the allow rule's
  pattern, but they are not blocked either: block rules are scoped
  to ad-network domains, and stream CDNs are not on the list.

## Adding a domain

Add the bare domain to the DOMAINS list and regenerate `ads.json`
with the generator (see git history for the command), keeping rule
ids unique and the allow rule at priority 100. Prefer domains over
URL patterns: a pattern broad enough to catch a rotating domain is
broad enough to catch a real stream. Verify in the browser with
`chrome.declarativeNetRequest.getMatchedRules` (the
`declarativeNetRequestFeedback` permission exists for this).
