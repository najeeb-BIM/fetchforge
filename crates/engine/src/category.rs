use std::path::Path;

/// Category used for automatic organisation into subfolders.
pub fn category_for_filename(filename: &str) -> &'static str {
    let ext = Path::new(filename)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" | "ts" | "m3u8" | "mpd" => "Video",
        "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" => "Audio",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "epub" => "Documents",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" => "Images",
        "zip" | "rar" | "7z" | "tar" | "gz" => "Archives",
        "exe" | "msi" | "dmg" | "iso" => "Software",
        _ => "Other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn categories() {
        assert_eq!(category_for_filename("film.mp4"), "Video");
        assert_eq!(category_for_filename("song.MP3"), "Audio");
        assert_eq!(category_for_filename("a.pdf"), "Documents");
        assert_eq!(category_for_filename("unknown.xyz"), "Other");
        assert_eq!(category_for_filename("no-extension"), "Other");
        assert_eq!(category_for_filename("CLIP.MP4"), "Video");
    }
}
