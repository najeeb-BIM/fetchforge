use std::path::{Path, PathBuf};

const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Sanitise a filename for Windows. Unicode is preserved; only characters
/// Windows forbids, control characters, trailing dots/spaces and reserved
/// device names are changed. Never returns a path separator, so the result
/// can never escape the download directory (path-traversal defence).
pub fn sanitize_filename(raw: &str) -> String {
    // Keep only the final path segment, whatever separator the source used.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let mut out: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    // Strip trailing dots / spaces (Windows silently mangles those).
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        out = "download".to_string();
    }
    // Reserved device names, with or without an extension.
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        out = format!("_{out}");
    }
    // Windows MAX_PATH-friendly cap: keep it well under 260 chars.
    if out.chars().count() > 200 {
        let mut truncated: String = out.chars().take(200).collect();
        while truncated.ends_with('.') || truncated.ends_with(' ') {
            truncated.pop();
        }
        out = truncated;
    }
    out
}

/// Decode a `Content-Disposition` filename, supporting both
/// `filename="..."` and RFC 5987 `filename*=UTF-8''...`.
pub fn filename_from_content_disposition(header: Option<&str>) -> Option<String> {
    let h = header?;
    for part in h.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename*=") {
            // Form: UTF-8''%E2%82%AC%20rates.pdf
            let rest = rest.trim_matches('"');
            let encoded = rest.splitn(3, '\'').nth(2).unwrap_or(rest);
            return Some(percent_decode(encoded));
        }
        if let Some(rest) = part.strip_prefix("filename=") {
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).replace('+', " ")
}

/// Filename from a URL path, or `None` if the path ends in `/` or has no name.
pub fn filename_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.next_back()?;
    if last.is_empty() {
        return None;
    }
    Some(percent_decode(last))
}

/// Pick a non-colliding destination inside `dir`: `file.zip`, `file (1).zip`, ...
pub fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let clean = sanitize_filename(filename);
    let candidate = dir.join(&clean);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(&clean)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| clean.clone());
    let ext = Path::new(&clean)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..10000 {
        let p = dir.join(format!("{stem} ({n}){ext}"));
        if !p.exists() {
            return p;
        }
    }
    dir.join(format!("{stem} (copy){ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_traversal_and_forbidden_chars() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("a<b>c:d\"e|f?g*h"), "a_b_c_d_e_f_g_h");
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("report."), "report");
        assert_eq!(sanitize_filename(""), "download");
        assert!(!sanitize_filename("..\\..\\win.ini").contains('\\'));
    }
    #[test]
    fn unicode_emoji_and_non_english_are_preserved() {
        // Non-English text and emoji are valid filenames on Windows and
        // must survive sanitising untouched (only forbidden chars change).
        assert_eq!(
            sanitize_filename("café 🎉 تقرير_2026.pdf"),
            "café 🎉 تقرير_2026.pdf"
        );
        assert_eq!(
            sanitize_filename("my file <script>.txt"),
            "my file _script_.txt"
        );
    }
    #[test]
    fn very_long_filename_is_truncated() {
        let long = format!("{}.pdf", "a".repeat(500));
        let clean = sanitize_filename(&long);
        assert!(
            clean.chars().count() <= 200,
            "got {}",
            clean.chars().count()
        );
        assert!(!clean.ends_with('.'));
    }
    #[test]
    fn unique_path_handles_duplicates() {
        let dir = std::env::temp_dir().join("fetchforge-unique-test");
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::remove_file(dir.join("dup.zip"));
        let _ = std::fs::remove_file(dir.join("dup (1).zip"));
        let _ = std::fs::remove_file(dir.join("dup (2).zip"));
        std::fs::write(dir.join("dup.zip"), b"x").unwrap();
        assert_eq!(
            unique_path(&dir, "dup.zip")
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "dup (1).zip"
        );
        std::fs::write(dir.join("dup (1).zip"), b"x").unwrap();
        assert_eq!(
            unique_path(&dir, "dup.zip")
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "dup (2).zip"
        );
        // Cleanup so the test is safe to run twice (idempotent).
        let _ = std::fs::remove_file(dir.join("dup.zip"));
        let _ = std::fs::remove_file(dir.join("dup (1).zip"));
        let _ = std::fs::remove_file(dir.join("dup (2).zip"));
    }
    #[test]
    fn windows_filename_rules() {
        // Reserved device names, with and without an extension, any case.
        for name in ["CON", "NUL", "AUX", "PRN"] {
            let clean = sanitize_filename(name);
            assert!(clean.starts_with('_'), "{name} -> {clean}");
            assert!(!clean.contains('/'));
        }
        for i in 1..=9 {
            for name in [format!("COM{i}"), format!("LPT{i}")] {
                let clean = sanitize_filename(&name);
                assert!(clean.starts_with('_'), "{name} -> {clean}");
                // With an extension the stem is still reserved.
                let with_ext = sanitize_filename(&format!("{name}.zip"));
                assert!(with_ext.starts_with('_'), "{name}.zip -> {with_ext}");
                assert!(with_ext.ends_with(".zip"));
            }
        }
        // Trailing dot or space (Windows silently mangles these).
        assert_eq!(sanitize_filename("report."), "report");
        assert_eq!(sanitize_filename("report "), "report");
        assert_eq!(sanitize_filename("report. "), "report");
        assert_eq!(sanitize_filename("report.."), "report");
        // Colon / Alternate Data Stream: the colon must not survive, so
        // "file.txt:stream" can never become an ADS on Windows.
        assert_eq!(sanitize_filename("report.txt:secret"), "report.txt_secret");
        assert!(!sanitize_filename("a:b:c").contains(':'));
        // Backslash and drive letters: only the final segment survives.
        assert_eq!(sanitize_filename(r"C:\Users\test\file.txt"), "file.txt");
        assert_eq!(sanitize_filename(r"D:\data\report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename("C:file.txt"), "C_file.txt");
        // Traversal, both separators, and empty / dot-only names.
        assert_eq!(sanitize_filename("../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename(r"..\..\Windows\win.ini"), "win.ini");
        assert_eq!(sanitize_filename("....//....//secret.txt"), "secret.txt");
        assert_eq!(sanitize_filename(""), "download");
        assert_eq!(sanitize_filename("."), "download");
        assert_eq!(sanitize_filename(".."), "download");
        // No result may ever contain a separator or a forbidden char.
        for raw in [
            "a/b/c.txt",
            r"a\b\c.txt",
            "a<b>c:d\"e|f?g*h",
            "CON",
            "file.txt:stream",
        ] {
            let clean = sanitize_filename(raw);
            assert!(
                !clean.contains('/') && !clean.contains('\\') && !clean.contains(':'),
                "{raw} -> {clean}"
            );
        }
    }
    #[test]
    fn long_name_keeps_total_path_under_260() {
        // Pure-function limit: the name itself is capped at 200 chars.
        // With a realistic 50-char download folder, the total path
        // (folder + separator + name) stays under Windows' 260 limit.
        let long = format!("{}.pdf", "a".repeat(500));
        let clean = sanitize_filename(&long);
        assert!(clean.chars().count() <= 200);
        let dir = Path::new("/tmp/downloads/0123456789abcdefghij0123456789ab");
        let total = unique_path(dir, &long);
        assert!(
            total.to_string_lossy().chars().count() < 260,
            "total path too long: {}",
            total.display()
        );
        // A 260+ char name on its own must still be truncated, not passed through.
        let very_long = "x".repeat(400);
        assert!(sanitize_filename(&very_long).chars().count() <= 200);
    }
    #[test]
    fn parses_content_disposition() {
        assert_eq!(
            filename_from_content_disposition(Some("attachment; filename=\"report 2026.pdf\""))
                .as_deref(),
            Some("report 2026.pdf")
        );
        assert_eq!(
            filename_from_content_disposition(Some("attachment; filename*=UTF-8''%E2%82%AC.pdf"))
                .as_deref(),
            Some("€.pdf")
        );
    }
}
