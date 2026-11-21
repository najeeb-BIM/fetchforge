//! Image downloading (spec §5): classification, srcset parsing, and
//! the extension fix for extension-less image URLs.
//!
//! Images are single files, so the actual download is the existing
//! direct path (`download.rs`): a probed image URL goes to
//! `add_direct_record` with category "Images" (see `category.rs`).
//! This module only supplies what the direct path cannot know on its
//! own: whether a URL / content type is an image, which srcset entry
//! is the full-resolution one, and which extension an extension-less
//! image URL should be saved with.
//!
//! Original format only: there is no PNG/JPG/WebP conversion. Adding
//! it would need the `image` crate (decoding + re-encoding, which can
//! silently lose quality), and the spec forbids advertising a converted
//! file as original quality. If conversion is wanted later, it belongs
//! here behind an explicit user choice, never as a default.
//! Nothing is ever upscaled.

use std::path::Path;

/// Image extensions the spec lists (§1, §5), plus BMP (already an
/// "Images" category in `category.rs`). `ico` is deliberately absent:
/// favicons are page furniture, not downloads.
pub const IMAGE_EXTENSIONS: [&str; 9] = [
    "jpg", "jpeg", "png", "webp", "gif", "svg", "avif", "bmp", "tif",
];

/// The extension of a URL's path (query string and fragment ignored,
/// case-insensitive). `None` when the URL has no extension.
pub fn url_path_extension(url: &str) -> Option<String> {
    let path = url::Url::parse(url)
        .map(|u| u.path().to_ascii_lowercase())
        .unwrap_or_else(|_| {
            url.split(['?', '#'])
                .next()
                .unwrap_or("")
                .to_ascii_lowercase()
        });
    Path::new(&path)
        .extension()
        .map(|e| e.to_string_lossy().to_string())
}

/// True when the URL points at an image file by extension.
pub fn is_image_url(url: &str) -> bool {
    url_path_extension(url)
        .is_some_and(|ext| IMAGE_EXTENSIONS.contains(&ext.as_str()))
}

/// True when a probed `Content-Type` is an image type (`image/*`,
/// parameters such as `; charset=...` ignored).
pub fn is_image_content_type(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .starts_with("image/")
}

/// The file extension for an image content type, or `None` when the
/// type is not a known image type. Used to give extension-less image
/// URLs a real extension (so the file opens and categorises as an
/// image instead of landing in "Other" with no extension).
pub fn image_ext_for_content_type(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "image/svg+xml" => Some("svg"),
        "image/avif" => Some("avif"),
        "image/bmp" => Some("bmp"),
        "image/tiff" => Some("tif"),
        "image/x-icon" | "image/vnd.microsoft.icon" => Some("ico"),
        _ => None,
    }
}

/// Give an image filename its extension when it has none. A filename
/// that already has any extension is returned unchanged (the server's
/// name wins; a mismatched content type must not rewrite it), and a
/// filename with an unknown content type is returned unchanged rather
/// than guessed at.
pub fn ensure_image_extension(filename: &str, content_type: Option<&str>) -> String {
    if Path::new(filename).extension().is_some() {
        return filename.to_string();
    }
    match content_type.and_then(image_ext_for_content_type) {
        Some(ext) => format!("{filename}.{ext}"),
        None => filename.to_string(),
    }
}

/// One entry of a `srcset` attribute: the URL plus its width (`800w`)
/// or density (`2x`) descriptor, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct SrcsetCandidate {
    pub url: String,
    pub width: Option<u32>,
    pub density: Option<f32>,
}

/// Parse a `srcset` attribute into candidates. Malformed entries
/// (no URL, or an unreadable descriptor) are skipped, never fatal:
/// a page with a broken srcset still yields its usable entries.
pub fn parse_srcset(srcset: &str) -> Vec<SrcsetCandidate> {
    let mut out = Vec::new();
    for part in srcset.split(',') {
        let mut tokens = part.split_whitespace();
        let Some(url) = tokens.next() else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        let mut width = None;
        let mut density = None;
        for token in tokens {
            if let Some(w) = token.strip_suffix('w') {
                if let Ok(n) = w.parse::<u32>() {
                    width = Some(n);
                }
            } else if let Some(d) = token.strip_suffix('x') {
                if let Ok(n) = d.parse::<f32>() {
                    density = Some(n);
                }
            }
            // Unknown descriptors are ignored, not fatal.
        }
        out.push(SrcsetCandidate {
            url: url.to_string(),
            width,
            density,
        });
    }
    out
}

/// The full-resolution URL of a `srcset`: the largest width when
/// widths are present, else the highest density, else the first
/// candidate. Ties prefer the later candidate (srcsets are normally
/// written smallest-first). `None` when the srcset has no usable
/// entry.
pub fn pick_srcset_url(srcset: &str) -> Option<String> {
    pick_best_srcset_url(&parse_srcset(srcset))
}

/// Same choice as [`pick_srcset_url`], over an already-parsed list
/// (used when several srcsets — `<img>` plus its `<picture>` sources
/// — are compared together).
pub fn pick_best_srcset_url(candidates: &[SrcsetCandidate]) -> Option<String> {
    let mut best: Option<&SrcsetCandidate> = None;
    for c in candidates {
        let Some(b) = best else {
            best = Some(c);
            continue;
        };
        match (c.width, b.width) {
            // Widths decide first; a width always beats a density-only
            // candidate (a width is an absolute size, a density is
            // relative to the display).
            (Some(cw), Some(bw)) => {
                if cw >= bw {
                    best = Some(c);
                }
            }
            (Some(_), None) => best = Some(c),
            (None, Some(_)) => {}
            (None, None) => match (c.density, b.density) {
                (Some(cd), Some(bd)) => {
                    if cd >= bd {
                        best = Some(c);
                    }
                }
                (Some(_), None) => best = Some(c),
                _ => {}
            },
        }
    }
    best.map(|c| c.url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_url_by_extension_ignores_query_and_case() {
        assert!(is_image_url("https://example.com/photo.JPG?x=1#frag"));
        assert!(is_image_url("https://example.com/a/b/image.webp"));
        assert!(is_image_url("https://example.com/avif-file.avif"));
        assert!(!is_image_url("https://example.com/photo.mp4"));
        assert!(!is_image_url("https://example.com/noext"));
        assert!(!is_image_url("https://example.com/photo.png.exe"));
    }

    #[test]
    fn image_content_types() {
        assert!(is_image_content_type("image/png"));
        assert!(is_image_content_type("image/jpeg; charset=binary"));
        assert!(!is_image_content_type("text/html"));
        assert!(!is_image_content_type("application/octet-stream"));
        assert_eq!(image_ext_for_content_type("image/jpeg"), Some("jpg"));
        assert_eq!(image_ext_for_content_type("image/svg+xml"), Some("svg"));
        assert_eq!(image_ext_for_content_type("image/avif"), Some("avif"));
        assert_eq!(image_ext_for_content_type("image/unknown"), None);
        assert_eq!(image_ext_for_content_type("text/html"), None);
    }

    #[test]
    fn extension_fix_only_when_missing() {
        assert_eq!(
            ensure_image_extension("photo", Some("image/jpeg")),
            "photo.jpg"
        );
        assert_eq!(
            ensure_image_extension("photo", Some("image/png")),
            "photo.png"
        );
        // A filename that already has an extension is never rewritten.
        assert_eq!(
            ensure_image_extension("photo.png", Some("image/jpeg")),
            "photo.png"
        );
        assert_eq!(
            ensure_image_extension("photo.bin", Some("image/jpeg")),
            "photo.bin"
        );
        // Unknown content type: no guessing.
        assert_eq!(ensure_image_extension("photo", None), "photo");
        assert_eq!(
            ensure_image_extension("photo", Some("text/html")),
            "photo"
        );
    }

    #[test]
    fn srcset_parses_width_and_density() {
        let c = parse_srcset("small.jpg 400w, big.jpg 1200w");
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].width, Some(400));
        assert_eq!(c[1].width, Some(1200));
        let d = parse_srcset("a.jpg 1x, b.jpg 2x, c.jpg 1.5x");
        assert_eq!(d[1].density, Some(2.0));
        assert_eq!(d[2].density, Some(1.5));
        // No descriptor: a plain URL is still a candidate.
        let plain = parse_srcset("only.jpg");
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].width, None);
        // Malformed entries are skipped, usable ones survive.
        let mixed = parse_srcset("good.jpg 800w, , bad.jpg notadescriptor, also.jpg 300w");
        assert_eq!(mixed.len(), 3);
        assert_eq!(parse_srcset(""), Vec::<SrcsetCandidate>::new());
        assert_eq!(parse_srcset("   ,  , "), Vec::<SrcsetCandidate>::new());
    }

    #[test]
    fn srcset_picks_largest() {
        assert_eq!(
            pick_srcset_url("small.jpg 400w, big.jpg 1200w, mid.jpg 800w"),
            Some("big.jpg".to_string())
        );
        assert_eq!(
            pick_srcset_url("a.jpg 1x, b.jpg 2x"),
            Some("b.jpg".to_string())
        );
        // A width beats a density-only candidate.
        assert_eq!(
            pick_srcset_url("a.jpg 2x, b.jpg 100w"),
            Some("b.jpg".to_string())
        );
        // No descriptors: first candidate.
        assert_eq!(
            pick_srcset_url("first.jpg, second.jpg"),
            Some("first.jpg".to_string())
        );
        // Equal widths: later candidate wins.
        assert_eq!(
            pick_srcset_url("a.jpg 800w, b.jpg 800w"),
            Some("b.jpg".to_string())
        );
        assert_eq!(pick_srcset_url(""), None);
        assert_eq!(pick_srcset_url(" , "), None);
    }
}
