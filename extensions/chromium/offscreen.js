// Offscreen document for clipboard reads (Chromium only, round 11
// extension, feature 6): a service worker cannot read the
// clipboard (no document focus), so this document — created by
// the background script only while clipboard monitoring is
// enabled, and only for the few seconds a poll takes — reads it
// on load and replies with the text. It never starts a download;
// the background only offers a media URL it finds.
(async () => {
  let text = null;
  try { text = await navigator.clipboard.readText(); } catch (_) { text = null; }
  try { chrome.runtime.sendMessage({ type: "clipboardText", text: text }); } catch (_) {}
})();
