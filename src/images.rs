//! Short-lived image files that a person attaches to a letter by absolute path.
use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub const MAX_BYTES: usize = 20 * 1024 * 1024;
pub const TTL: Duration = Duration::from_hours(24);
const PREFIX: &str = "image-";

/// Dedicated cache directory; nothing else is ever written or removed here.
pub fn directory(home: &Path) -> PathBuf {
    home.join(".cache/agent-talk/images")
}

/// Accepted MIME type for an upload's `Content-Type`, or `None`.
pub fn declared_type(content_type: &str) -> Option<&'static str> {
    let essence = content_type.split(';').next().unwrap_or_default().trim();
    ["image/png", "image/jpeg", "image/webp"]
        .into_iter()
        .find(|accepted| essence.eq_ignore_ascii_case(accepted))
}

/// File extension proven by the content's signature, independent of the declared type.
pub fn kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// Store under a new server-generated name without replacing any file.
pub fn save(dir: &Path, bytes: &[u8], extension: &str) -> io::Result<PathBuf> {
    use std::{io::Write, os::unix::fs::DirBuilderExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // O_EXCL creation with a random name; the file is removed if writing fails.
    let mut file = tempfile::Builder::new()
        .prefix(PREFIX)
        .suffix(&format!(".{extension}"))
        .rand_bytes(16)
        .tempfile_in(dir.canonicalize()?)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    let (_, path) = file.keep().map_err(|error| error.error)?;
    Ok(path)
}

/// Remove this directory's own expired images. Returns how many were removed.
pub fn sweep(dir: &Path, now: SystemTime) -> io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        entries => entries?,
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let owned = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(PREFIX));
        // DirEntry metadata does not follow symlinks.
        let metadata = entry.metadata()?;
        if owned
            && metadata.is_file()
            && now
                .duration_since(metadata.modified()?)
                .is_ok_and(|age| age >= TTL)
        {
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF\0";
    const WEBP: &[u8] = b"RIFF\x24\0\0\0WEBPVP8 ";

    #[test]
    fn kind_trusts_only_signatures_of_supported_formats() {
        assert_eq!(kind(PNG), Some("png"));
        assert_eq!(kind(JPEG), Some("jpg"));
        assert_eq!(kind(WEBP), Some("webp"));
        for rejected in [
            &b""[..],
            b"\x89PNG",
            b"GIF89a\x01\0\x01\0",
            b"RIFF\x24\0\0\0WAVEfmt ",
            b"\0\0\0\x18ftypheic\0\0\0\0",
            b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>",
            b"%PDF-1.7",
        ] {
            assert_eq!(kind(rejected), None, "{rejected:?}");
        }
    }

    #[test]
    fn declared_type_accepts_only_supported_image_types() {
        assert_eq!(declared_type("image/png"), Some("image/png"));
        assert_eq!(declared_type("IMAGE/JPEG; x=1"), Some("image/jpeg"));
        assert_eq!(declared_type(" image/webp "), Some("image/webp"));
        for rejected in ["", "image/heic", "image/gif", "text/plain", "image/svg+xml"] {
            assert_eq!(declared_type(rejected), None, "{rejected}");
        }
    }

    #[test]
    fn save_uses_new_private_names_and_never_replaces_files() {
        let home = tempfile::tempdir().unwrap();
        let dir = directory(home.path());
        let first = save(&dir, PNG, "png").unwrap();
        let second = save(&dir, PNG, "png").unwrap();
        assert_ne!(first, second);
        for path in [&first, &second] {
            assert!(path.is_absolute());
            assert_eq!(path.parent().unwrap(), dir.canonicalize().unwrap());
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(name.starts_with(PREFIX), "{name}");
            assert_eq!(path.extension().unwrap(), "png");
            assert_eq!(std::fs::read(path).unwrap(), PNG);
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "{mode:o}");
        }
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }

    #[test]
    fn sweep_removes_only_expired_images_it_owns() {
        let home = tempfile::tempdir().unwrap();
        let dir = directory(home.path());
        let image = save(&dir, PNG, "png").unwrap();
        let foreign = dir.join("notes.png");
        std::fs::write(&foreign, PNG).unwrap();
        let outside = home.path().join("image-outside.png");
        std::fs::write(&outside, PNG).unwrap();
        let link = dir.join("image-link.png");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let nested = dir.join("image-dir.png");
        std::fs::create_dir(&nested).unwrap();
        let written = std::fs::metadata(&image).unwrap().modified().unwrap();

        assert_eq!(
            sweep(&dir, written + TTL - Duration::from_secs(1)).unwrap(),
            0
        );
        assert!(image.exists());
        assert_eq!(sweep(&dir, written + TTL).unwrap(), 1);
        assert!(!image.exists());
        for kept in [&foreign, &outside, &nested] {
            assert!(kept.exists(), "{}", kept.display());
        }
        assert!(link.symlink_metadata().is_ok());
    }

    #[test]
    fn sweep_treats_a_missing_directory_as_empty() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            sweep(&directory(home.path()), SystemTime::now()).unwrap(),
            0
        );
    }
}
