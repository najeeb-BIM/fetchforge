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
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "avif" | "bmp" => "Images",
        "glb" | "gltf" | "obj" | "fbx" | "stl" | "usdz" => "3D",
        "zip" | "rar" | "7z" | "tar" | "gz" => "Archives",
        "exe" | "msi" | "dmg" | "iso" => "Software",
        _ => "Other",
    }
}

/// Category for a download whose final form is known: an
/// audio-only download is Audio no matter what the intermediate
/// filename says (round 13, item 6 — NOXX 2026-11-25: an
/// audio-only `.m4a` landed in Video/ because the record still
/// carried the video format's `.mp4` filename when the folder
/// was chosen). Callers that know `audio_only` should use this
/// instead of [`category_for_filename`].
pub fn category_for_download(filename: &str, audio_only: bool) -> &'static str {
    if audio_only {
        "Audio"
    } else {
        category_for_filename(filename)
    }
}

/// Is this folder name one of the category subfolders
/// ([`category_for_filename`])? Used to recognise a
/// caller-computed category dir (see
/// [`audio_destination_dir`]).
fn is_category_dir(name: &str) -> bool {
    matches!(
        name,
        "Video" | "Audio" | "Documents" | "Images" | "3D" | "Archives" | "Software" | "Other"
    )
}

/// Re-route a caller-computed destination dir for an audio-only
/// download: when the dir ends in a category subfolder (the
/// caller organised by the intermediate filename — e.g. Video/
/// for a `.mp4` name), swap it for Audio/, so both the
/// intermediate file and the extracted audio land in Audio/.
/// A dir that does not end in a category folder (organising
/// off, or a user folder with another name) is returned
/// unchanged.
///
/// Edge: a download root literally named like a category
/// (e.g. `D:\Video`) with organising off is also swapped —
/// the caller computed no category, but the name is
/// indistinguishable from one. The desktop caller should
/// prefer [`category_for_download`] when it knows `audio_only`,
/// which never guesses from the dir name.
pub fn audio_destination_dir(dir: &Path) -> std::path::PathBuf {
    match dir.file_name().and_then(|n| n.to_str()) {
        Some(name) if is_category_dir(name) && name != "Audio" => dir.with_file_name("Audio"),
        _ => dir.to_path_buf(),
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
        assert_eq!(category_for_filename("photo.avif"), "Images");
        assert_eq!(category_for_filename("model.glb"), "3D");
        assert_eq!(category_for_filename("scene.GLTF"), "3D");
        assert_eq!(category_for_filename("part.stl"), "3D");
        assert_eq!(category_for_filename("unknown.xyz"), "Other");
        assert_eq!(category_for_filename("no-extension"), "Other");
        assert_eq!(category_for_filename("CLIP.MP4"), "Video");
    }

    #[test]
    fn audio_only_category_ignores_the_video_filename() {
        // Round 13, item 6: an audio-only download is Audio even
        // when the record still carries the video format's
        // filename — the folder is decided by what the download
        // IS, not by the intermediate name.
        assert_eq!(category_for_download("film.mp4", true), "Audio");
        assert_eq!(category_for_download("film.m4a", true), "Audio");
        assert_eq!(category_for_download("film.mp4", false), "Video");
        assert_eq!(category_for_download("song.mp3", false), "Audio");
    }

    #[test]
    fn audio_destination_dir_swaps_only_category_folders() {
        // A caller-computed Video/ (or any other category) dir
        // becomes Audio/; an already-Audio dir and a plain user
        // folder are left alone.
        assert_eq!(
            audio_destination_dir(Path::new("/dl/Video")),
            Path::new("/dl/Audio")
        );
        assert_eq!(
            audio_destination_dir(Path::new("/dl/Documents")),
            Path::new("/dl/Audio")
        );
        assert_eq!(
            audio_destination_dir(Path::new("/dl/Audio")),
            Path::new("/dl/Audio")
        );
        assert_eq!(
            audio_destination_dir(Path::new("/dl/Downloads")),
            Path::new("/dl/Downloads")
        );
        assert_eq!(audio_destination_dir(Path::new("/dl")), Path::new("/dl"));
    }
}
