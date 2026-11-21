# Universal Media Detection and Download Manager — Build Spec

User-ordered spec, 2026-11-21. This is the governing spec for the FetchForge
"download from anywhere" build. Implement phase by phase against the existing
codebase; preserve working functionality; do not rebuild unnecessarily.

## Objective

Upgrade FetchForge (local, IDM-style) from direct-URL downloading into a
universal media detection and download manager: detect accessible media
playing inside supported websites, show real available qualities, download
authorized video, audio, images and 3D assets, support HLS and DASH, merge
separate audio/video tracks, pause/resume/accelerate, integrate with browsers.

## 1. Universal Media Detection Engine

Detect media from websites when a user visits a page or plays media.
Formats: MP4, WebM, MKV, MOV, AVI, M4V, MP3, WAV, FLAC, AAC, OGG, M3U8 (HLS),
MPD (DASH), TS segments, images (JPG, JPEG, PNG, WEBP, GIF, SVG, AVIF),
3D (GLB, GLTF, OBJ, FBX, STL, USDZ), other downloadable media/documents.
Detect via: direct media URLs, browser network requests, HTML5 video/audio
elements, media source elements, blob URLs where accessible, media manifests,
JS-generated media requests, website media player resources, browser
extension / desktop browser integration. Do NOT rely only on HTML source
inspection — implement network-level resource detection and browser
integration.

## 2. Universal Video Downloader

When a video plays on a supported website, detect its downloadable streams
and show them in the download panel. Support: direct video files, HLS,
MPEG-DASH, multiple qualities, separate audio/video tracks, adaptive
bitrate, supported live streams where recording is permitted, subtitles
and captions. Show only qualities actually available (144p–4320p 8K).
User selects: quality, resolution, format, audio quality, video-only,
audio-only, combined, original stream. Use FFmpeg to merge separate
audio/video streams without unnecessary quality loss.

## 3. Smart Media Detection

Background detection: on page open, detect media resources, identify MIME
types, analyze network responses, identify manifests, extract stream info,
detect resolutions/formats, estimate file size when possible, send detected
media to the UI. Notification "Media detected!" with thumbnail, website
name, media title, format, available resolutions, estimated size, download
button. Avoid duplicates; ignore tracking, ads, analytics.

## 4. Advanced Download Engine

IDM-style: multithreaded, HTTP Range, parallel connections, acceleration,
pause/resume, retry, auto connection recovery, queue, scheduling, speed
control, bandwidth limits, simultaneous downloads, persistent history,
automatic filename detection, integrity verification, checksum where
available, temp files, resume interrupted downloads when supported. Never
assume Range/parallel support — detect server capabilities, fall back
gracefully.

## 5. Universal Image Downloader

Single + bulk image downloading, full-resolution detection, srcset
extraction, lazy-loaded images, WebP/AVIF, galleries. Options: original
resolution, available resolutions, original format, PNG, JPG, WebP. Never
upscale and advertise as original quality.

## 6. 3D Media Downloader

GLB, GLTF, OBJ, FBX, STL, USDZ, textures, materials, associated
dependencies. Preserve folder structures/dependencies where possible.
Detect assets loaded by supported 3D viewers/WebGL apps where technically
accessible. Do NOT bypass protected asset delivery or DRM.

## 7. Browser Integration

Chrome, Edge, Firefox, Chromium browsers. Extension, native messaging
where necessary, network request monitoring, media resource detection,
download interception, context menu, send-to-download-manager. Floating
download button near supported HTML5 media players when technically
possible. Toolbar icon showing count of detected media resources.

## 8. Advanced Media Information Panel

Per detected resource: title, domain, format, MIME type, size, resolution,
frame rate (when available), video codec, audio codec, bitrate, duration,
estimated download time, available quality options. Only show confirmed
metadata as fact.

## 9. User Interface

Preserve current UI style where possible. IDM-inspired: Main Dashboard,
Download Manager, Media Detector, Video Downloads, Image Downloads,
3D Assets, Download Queue, Download History, Browser Integration,
Settings. Dedicated MEDIA DETECTION panel auto-listing media from
connected browsers. Multi-select detected files for batch download.

## 10. Security and Accessibility

Local-first, legitimate access only. Do NOT bypass DRM, authentication,
paywalls, encryption, or protected streaming. Do NOT steal session
credentials or cookies. Respect access restrictions and copyright law.
Handle browser auth securely. Never expose sensitive headers/tokens in
logs. When media is protected/inaccessible, explain why clearly instead
of pretending success.

## 11. Technical Architecture

Build on the existing codebase (Tauri + Rust + bridge + extensions).
Consider FFmpeg, yt-dlp for supported/authorized sources, HTTP libraries,
persistent local database, background workers. Avoid unnecessary
dependencies. Modular components: MediaDetectionEngine, NetworkMonitor,
StreamAnalyzer, HLSParser, DASHParser, MediaMetadataExtractor,
VideoDownloadEngine, ImageDownloadEngine, ThreeDAssetDownloader,
DownloadQueueManager, BrowserIntegration, FFmpegProcessor,
DownloadStorageManager.

## 12. Implementation Workflow

- PHASE 1: Inspect existing codebase, identify current download architecture.
- PHASE 2: Identify why direct links work but embedded website media does not.
- PHASE 3: Design the integration architecture.
- PHASE 4: Implement network-level media detection.
- PHASE 5: Implement HLS and MPEG-DASH analysis/downloading (accessible, authorized streams).
- PHASE 6: Implement quality selection and audio/video merging.
- PHASE 7: Implement image and 3D asset detection.
- PHASE 8: Integrate browser extensions and native messaging where required.
- PHASE 9: Connect all new functionality to the existing UI.
- PHASE 10: Test the entire system and fix errors.

## 13. Testing Requirements

Test against authorized test sources: direct MP4 URLs, HTML5 video
players, HLS test streams, DASH test streams, audio players, image
galleries, lazy-loaded images, WebGL 3D viewers, multiple simultaneous
downloads, interrupted/resumed downloads, servers without Range support,
separate audio/video tracks. Automated tests wherever possible. Verify
detected media can actually be downloaded before showing success.

## 14. Development Rules

1. Implement real working functionality, not explanations or pseudocode.
2. Do not break existing download functionality.
3. Do not replace working modules without a valid reason.
4. No fake download buttons or non-functional UI.
5. Do not hardcode example media URLs into production logic.
6. Handle errors gracefully.
7. Modular, maintainable code.
8. Local-first.
9. Do not claim universal compatibility without testing it.
10. If a feature needs browser permissions, extensions or native messaging,
    implement the required integration instead of pretending it works.

At the end of each phase: explain what was implemented, list modified and
created files, report dependencies added, run relevant tests, report
remaining limitations.
