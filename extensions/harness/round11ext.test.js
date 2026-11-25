// Round 11 extension harness: the 14 downloader-study features'
// extension-side units. Every function under test is the exact
// source extracted from extensions/chromium/content.js or
// background.js — see extract.js.
//   - parseHlsMaster keeps ALL audio renditions per group
//     (feature 4: novahd carries 7 tracks in one group) and
//     exposes subtitle languages (feature 2).
//   - extractMediaUrlFromText finds the first media URL in
//     pasted text (feature 6: clipboard monitoring).
//   - isInterceptableDownload / downloadItemBasename decide
//     which browser downloads are media (feature 14).
//   - sortFormatsForPopup keeps same-bitrate audio formats in
//     different languages as separate rows (feature 4).
"use strict";
const { readSource, extractMany, extractConst, makeContext, runIn, test, assert, assertEq, run } = require("./extract");

const contentSrc = readSource("extensions/chromium/content.js");
const bgSrc = readSource("extensions/chromium/background.js");

// ---------- Context A: HLS master parsing + format ladder ----------
const ctxA = makeContext(
  {},
  [extractMany(contentSrc, ["parseHlsMaster", "parseHlsAttrs", "resolveAgainst", "friendlyCodec", "formatBandwidth", "hlsVariantHeight", "sortFormatsForPopup", "visibleFormatLabel", "effectiveFormatHeight"], 2)]
);

// novahd-style master: 7 audio renditions in ONE group, 2
// subtitle renditions, 2 variants sharing the audio group.
const NOVAHD_MASTER = [
  "#EXTM3U",
  "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio-en.m3u8\"",
  "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Spanish\",LANGUAGE=\"es\",URI=\"audio-es.m3u8\"",
  "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"French\",LANGUAGE=\"fr\",URI=\"audio-fr.m3u8\"",
  "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"German\",LANGUAGE=\"de\",URI=\"audio-de.m3u8\"",
  "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Italian\",LANGUAGE=\"it\",URI=\"audio-it.m3u8\"",
  "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Portuguese\",LANGUAGE=\"pt\",URI=\"audio-pt.m3u8\"",
  "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Hindi\",LANGUAGE=\"hi\",URI=\"audio-hi.m3u8\"",
  "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",URI=\"subs-en.m3u8\"",
  "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Spanish\",LANGUAGE=\"es\",URI=\"subs-es.m3u8\"",
  "#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028,mp4a.40.2\",AUDIO=\"aud\",SUBTITLES=\"subs\"",
  "video-1080.m3u8",
  "#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\",AUDIO=\"aud\",SUBTITLES=\"subs\"",
  "video-720.m3u8",
  "",
].join("\n");
const MASTER_URL = "https://cdn.example.com/film/master.m3u8";

test("Feature 4: parseHlsMaster keeps all 7 audio renditions in one group", () => {
  ctxA.text = NOVAHD_MASTER;
  const p = runIn(ctxA, "parseHlsMaster(text, \"" + MASTER_URL + "\")");
  assert(p, "master parses");
  assertEq(p.audioGroups.aud.length, 7, "7 renditions kept");
  const audioRows = p.rows.filter((r) => r.audioOnly);
  assertEq(audioRows.length, 7, "7 audio-only rows");
  assertEq(p.audioLanguages.length, 7, "7 languages listed");
  assert(p.audioLanguages.indexOf("en") !== -1, "en listed");
  assert(p.audioLanguages.indexOf("hi") !== -1, "hi listed");
  assertEq(audioRows[1].audioLang, "es", "row carries its language");
  assertEq(audioRows[1].url, "https://cdn.example.com/film/audio-es.m3u8", "row URL resolves");
  assert(p.hasAudioRows, "hasAudioRows true");
});

test("Feature 4: variant rows default to the group's first rendition and carry the group id", () => {
  ctxA.text = NOVAHD_MASTER;
  const p = runIn(ctxA, "parseHlsMaster(text, \"" + MASTER_URL + "\")");
  const videoRows = p.rows.filter((r) => !r.audioOnly);
  assertEq(videoRows.length, 2, "2 video rows");
  assertEq(videoRows[0].audioUrl, "https://cdn.example.com/film/audio-en.m3u8", "default audio is the first rendition");
  assertEq(videoRows[0].audioGroup, "aud", "group id carried for pick-time resolution");
  assertEq(videoRows[0].label.indexOf("Best"), 0, "top row is Best");
});

test("Feature 2: parseHlsMaster exposes subtitle languages", () => {
  ctxA.text = NOVAHD_MASTER;
  const p = runIn(ctxA, "parseHlsMaster(text, \"" + MASTER_URL + "\")");
  assertEq(p.subtitleLanguages.length, 2, "2 subtitle languages");
  assert(p.subtitleLanguages.indexOf("en") !== -1, "en subtitles");
  assert(p.subtitleLanguages.indexOf("es") !== -1, "es subtitles");
});

test("Feature 4: a single untagged audio track yields no language select", () => {
  const master = [
    "#EXTM3U",
    "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Audio\",URI=\"audio.m3u8\"",
    "#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,AUDIO=\"aud\"",
    "video.m3u8",
    "",
  ].join("\n");
  ctxA.text = master;
  const p = runIn(ctxA, "parseHlsMaster(text, \"" + MASTER_URL + "\")");
  assertEq(p.audioLanguages.length, 1, "one language (the NAME fallback)");
  assertEq(p.subtitleLanguages.length, 0, "no subtitles");
});

test("Feature 4: sortFormatsForPopup keeps same-bitrate audio in different languages", () => {
  const formats = [
    { format_id: "140-en", label: "Audio only · m4a · English", kind: "audio_only", abr: 128, language: "en" },
    { format_id: "140-es", label: "Audio only · m4a · Spanish", kind: "audio_only", abr: 128, language: "es" },
    { format_id: "137", label: "1080p · mp4 · video only", kind: "video_only", height: 1080, vbr: 4500 },
  ];
  ctxA.formats = formats;
  const sorted = runIn(ctxA, "sortFormatsForPopup(formats)");
  assertEq(sorted.length, 3, "no language row deduped away");
  assertEq(sorted[0].format_id, "137", "video still first");
  assertEq(sorted[1].kind, "audio_only", "audio rows last");
  assertEq(sorted[2].kind, "audio_only", "audio rows last");
});

// ---------- Context B: clipboard + interception (background.js) ----------
const ctxB = makeContext(
  {},
  [
    extractConst(bgSrc, "CLIPBOARD_MEDIA_HOSTS", 0),
    extractConst(bgSrc, "CLIPBOARD_MEDIA_EXTENSIONS", 0),
    extractConst(bgSrc, "INTERCEPT_EXTENSIONS", 0),
    extractMany(bgSrc, ["extractMediaUrlFromText", "isInterceptableDownload", "downloadItemBasename"], 0),
  ]
);

test("Feature 6: extractMediaUrlFromText finds a YouTube link in surrounding text", () => {
  assertEq(runIn(ctxB, "extractMediaUrlFromText(\"watch this https://www.youtube.com/watch?v=aqz-KE-bpKQ it is great\")"), "https://www.youtube.com/watch?v=aqz-KE-bpKQ");
});

test("Feature 6: extractMediaUrlFromText strips trailing sentence punctuation", () => {
  assertEq(runIn(ctxB, "extractMediaUrlFromText(\"link: https://example.com/film.mp4.\")"), "https://example.com/film.mp4");
  assertEq(runIn(ctxB, "extractMediaUrlFromText(\"(see https://example.com/film.mp4)\")"), "https://example.com/film.mp4");
});

test("Feature 6: extractMediaUrlFromText skips non-media URLs and returns null when none", () => {
  assertEq(runIn(ctxB, "extractMediaUrlFromText(\"https://example.com/about\")"), null);
  assertEq(runIn(ctxB, "extractMediaUrlFromText(\"no links here\")"), null);
  assertEq(runIn(ctxB, "extractMediaUrlFromText(\"\")"), null);
  assertEq(runIn(ctxB, "extractMediaUrlFromText(null)"), null);
});

test("Feature 6: extractMediaUrlFromText takes the first media URL, not the first URL", () => {
  assertEq(
    runIn(ctxB, "extractMediaUrlFromText(\"https://example.com/about then https://vimeo.com/12345\")"),
    "https://vimeo.com/12345"
  );
  assertEq(
    runIn(ctxB, "extractMediaUrlFromText(\"https://cdn.example.com/stream.m3u8\")"),
    "https://cdn.example.com/stream.m3u8"
  );
});

test("Feature 14: isInterceptableDownload matches media MIME and extension", () => {
  assert(runIn(ctxB, "isInterceptableDownload({ url: \"https://cdn.example.com/dl/abc\", mime: \"video/mp4\" })"), "video MIME, extensionless URL");
  assert(runIn(ctxB, "isInterceptableDownload({ url: \"https://cdn.example.com/song.mp3\", mime: \"\" })"), "mp3 extension");
  assert(runIn(ctxB, "isInterceptableDownload({ url: \"https://cdn.example.com/film.MKV\" })"), "extension case-insensitive");
  assert(!runIn(ctxB, "isInterceptableDownload({ url: \"https://example.com/doc.pdf\", mime: \"application/pdf\" })"), "pdf is not media");
  assert(!runIn(ctxB, "isInterceptableDownload({ url: \"https://example.com/page\", mime: \"text/html\" })"), "html page is not media");
  assert(!runIn(ctxB, "isInterceptableDownload({ url: \"ftp://example.com/film.mp4\" })"), "non-http(s) rejected");
  assert(!runIn(ctxB, "isInterceptableDownload(null)"), "null rejected");
});

test("Feature 14: downloadItemBasename reduces a full path to its basename", () => {
  assertEq(runIn(ctxB, "downloadItemBasename({ filename: \"C:\\\\Users\\\\najo2\\\\Downloads\\\\The Odyssey (2026).mp4\" })"), "The Odyssey (2026).mp4");
  assertEq(runIn(ctxB, "downloadItemBasename({ filename: \"/home/u/Downloads/song%20one.mp3\" })"), "song one.mp3");
  assertEq(runIn(ctxB, "downloadItemBasename({ url: \"https://cdn.example.com/film.mkv\" })"), "film.mkv");
});

run("round11ext.test.js");
