use std::{
    env,
    fs::{self, File},
    io::Read,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tar::Archive;
use tempfile::{NamedTempFile, TempDir};

use crate::{config::Config, lifecycle};

const REPOSITORY: &str = "miyabi-sunny-side/agent-talkd";
const RELEASE_API: &str =
    "https://api.github.com/repos/miyabi-sunny-side/agent-talkd/releases/latest";
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Deserialize)]
struct LatestRelease {
    tag_name: String,
}

trait Downloader {
    fn download(&self, url: &str, destination: &Path) -> Result<()>;
}

struct CurlDownloader;

impl Downloader for CurlDownloader {
    fn download(&self, url: &str, destination: &Path) -> Result<()> {
        let output = Command::new("curl")
            .args([
                "--proto",
                "=https",
                "--tlsv1.2",
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--header",
                "Accept: application/vnd.github+json",
                "--header",
                "X-GitHub-Api-Version: 2022-11-28",
                "--output",
            ])
            .arg(destination)
            .arg(url)
            .output()
            .with_context(|| format!("cannot run curl for {url}"))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            bail!("download failed for {url}: {}", detail.trim().to_owned());
        }
        Ok(())
    }
}

pub async fn run() -> Result<i32> {
    let target = update_target()?;
    let current_text = env!("CARGO_PKG_VERSION");
    let current = match Version::parse(current_text) {
        Ok(version) => version,
        Err(error) => {
            eprintln!(
                "agent-talk: local version '{current_text}' is not valid semver; update skipped: {error}"
            );
            reconcile(&target, current_text).await?;
            return Ok(0);
        }
    };
    let workspace = tempfile::tempdir()?;
    let remote = fetch_latest(&CurlDownloader, &workspace)?;

    if remote <= current {
        println!(
            "agent-talk: already current (local {current}, latest {remote}); binary unchanged"
        );
        reconcile(&target, current_text).await?;
        return Ok(0);
    }

    let asset = platform_asset()?;
    let extracted = download_and_verify(&CurlDownloader, &workspace, &remote, asset)?;
    atomic_replace(&target, &extracted)?;
    println!("agent-talk: updated {current} -> {remote}");
    reconcile(&target, &remote.to_string()).await?;
    Ok(0)
}

fn update_target() -> Result<PathBuf> {
    let target = env::current_exe()?.canonicalize()?;
    let metadata = fs::symlink_metadata(&target)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!(
            "refusing to update a non-regular executable: {}",
            target.display()
        );
    }
    Ok(target)
}

fn fetch_latest(downloader: &impl Downloader, workspace: &TempDir) -> Result<Version> {
    let metadata_path = workspace.path().join("latest.json");
    downloader
        .download(RELEASE_API, &metadata_path)
        .context("GitHub latest release lookup failed")?;
    let release: LatestRelease = serde_json::from_reader(File::open(metadata_path)?)
        .context("GitHub latest release response is invalid")?;
    parse_release_tag(&release.tag_name)
}

fn parse_release_tag(tag: &str) -> Result<Version> {
    let value = tag
        .strip_prefix('v')
        .context("latest release tag must start with 'v'")?;
    let version = Version::parse(value).context("latest release tag is not valid semver")?;
    if !version.pre.is_empty()
        || !version.build.is_empty()
        || tag != format!("v{}.{}.{}", version.major, version.minor, version.patch)
    {
        bail!("latest release tag is not a stable vMAJOR.MINOR.PATCH tag");
    }
    Ok(version)
}

fn platform_asset() -> Result<&'static str> {
    match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => Ok("agent-talk-linux-x86_64.tar.gz"),
        ("macos", "aarch64") => Ok("agent-talk-macos-aarch64.tar.gz"),
        (os, arch) => bail!("self-update is not supported on {os}/{arch}"),
    }
}

fn download_and_verify(
    downloader: &impl Downloader,
    workspace: &TempDir,
    version: &Version,
    asset: &str,
) -> Result<PathBuf> {
    let base = format!("https://github.com/{REPOSITORY}/releases/download/v{version}");
    let archive_path = workspace.path().join(asset);
    let checksum_path = workspace.path().join(format!("{asset}.sha256"));
    downloader.download(&format!("{base}/{asset}"), &archive_path)?;
    downloader.download(&format!("{base}/{asset}.sha256"), &checksum_path)?;

    let size = fs::metadata(&archive_path)?.len();
    if size == 0 || size > MAX_ARCHIVE_BYTES {
        bail!("release archive has an invalid size: {size} bytes");
    }
    verify_checksum(&archive_path, &checksum_path, asset)?;
    extract_binary(&archive_path, workspace.path())
}

fn verify_checksum(archive: &Path, checksum: &Path, asset: &str) -> Result<()> {
    let text = fs::read_to_string(checksum)?;
    let mut fields = text.split_whitespace();
    let expected = fields.next().context("checksum file is empty")?;
    let filename = fields.next().context("checksum file has no filename")?;
    if fields.next().is_some()
        || expected.len() != 64
        || !expected.bytes().all(|byte| byte.is_ascii_hexdigit())
        || filename.trim_start_matches('*') != asset
    {
        bail!("checksum file has an invalid format");
    }

    let mut source = File::open(archive)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("release checksum mismatch");
    }
    Ok(())
}

fn extract_binary(archive: &Path, destination: &Path) -> Result<PathBuf> {
    let decoder = GzDecoder::new(File::open(archive)?);
    let mut archive = Archive::new(decoder);
    let binary = destination.join("verified-agent-talk");
    let mut found = false;
    let mut total_size = 0_u64;

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !safe_archive_path(&path) {
            bail!("release archive contains an unsafe path");
        }
        if !entry.header().entry_type().is_file() {
            bail!("release archive contains a non-regular entry");
        }
        let size = entry.size();
        total_size = total_size
            .checked_add(size)
            .context("release archive size overflow")?;
        if total_size > MAX_ARCHIVE_BYTES {
            bail!("release archive expands beyond the size limit");
        }
        if path == Path::new("agent-talk") {
            if found || size == 0 || size > MAX_BINARY_BYTES {
                bail!("release archive contains an invalid agent-talk binary");
            }
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&binary)?;
            std::io::copy(&mut entry, &mut output)?;
            output.sync_all()?;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
            found = true;
        }
    }
    if !found {
        bail!("release archive does not contain agent-talk");
    }
    Ok(binary)
}

fn safe_archive_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn atomic_replace(target: &Path, source: &Path) -> Result<()> {
    let parent = target.parent().context("executable parent is missing")?;
    let metadata = fs::symlink_metadata(target)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.file_type().is_socket()
    {
        bail!("refusing to replace a non-regular executable");
    }

    let mut staged = NamedTempFile::new_in(parent)?;
    let mut input = File::open(source)?;
    std::io::copy(&mut input, staged.as_file_mut())?;
    staged
        .as_file()
        .set_permissions(fs::Permissions::from_mode(metadata.permissions().mode()))?;
    staged.as_file().sync_all()?;
    staged
        .persist(target)
        .map_err(|error| error.error)
        .context("atomic executable replacement failed")?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

async fn reconcile(target: &Path, expected: &str) -> Result<()> {
    lifecycle::executable_matches_version(target, expected)?;
    let output = Command::new(target)
        .arg("ensure-daemon")
        .output()
        .context("cannot run the updated binary's ensure-daemon")?;
    if !output.status.success() {
        bail!(
            "binary is installed, but daemon reconciliation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if !output.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }

    if let Some(config) = Config::discover_optional()? {
        let status = lifecycle::daemon_status(&config)
            .await
            .context("binary is installed, but daemon postcondition failed")?;
        if !status.ready || status.version != expected {
            bail!(
                "binary is installed, but daemon version is {} instead of {expected}",
                status.version
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, io::Cursor};

    use super::*;

    struct FakeDownloader {
        files: BTreeMap<String, Vec<u8>>,
    }

    impl Downloader for FakeDownloader {
        fn download(&self, url: &str, destination: &Path) -> Result<()> {
            let body = self.files.get(url).context("injected download failure")?;
            fs::write(destination, body)?;
            Ok(())
        }
    }

    #[test]
    fn stable_release_tags_are_strictly_parsed() {
        assert_eq!(parse_release_tag("v1.2.3").unwrap(), Version::new(1, 2, 3));
        for invalid in ["1.2.3", "v1.2", "v1.2.3-dev", "v01.2.3", "v1.2.3+build"] {
            assert!(parse_release_tag(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    #[ignore = "requires the public GitHub API"]
    fn public_latest_release_is_reachable() {
        let workspace = tempfile::tempdir().unwrap();
        let version = fetch_latest(&CurlDownloader, &workspace).unwrap();
        assert!(version.major > 0 || version.minor > 0 || version.patch > 0);
    }

    #[test]
    fn download_failure_is_reported_before_installation() {
        let workspace = tempfile::tempdir().unwrap();
        let error = fetch_latest(
            &FakeDownloader {
                files: BTreeMap::new(),
            },
            &workspace,
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("GitHub latest release lookup failed")
        );
    }

    #[test]
    fn bad_checksum_does_not_touch_existing_binary() {
        let workspace = tempfile::tempdir().unwrap();
        let archive = workspace.path().join("asset.tar.gz");
        let checksum = workspace.path().join("asset.tar.gz.sha256");
        fs::write(&archive, b"archive").unwrap();
        fs::write(&checksum, format!("{}  asset.tar.gz\n", "0".repeat(64))).unwrap();
        let target = workspace.path().join("agent-talk");
        fs::write(&target, b"old").unwrap();

        assert!(verify_checksum(&archive, &checksum, "asset.tar.gz").is_err());
        assert_eq!(fs::read(target).unwrap(), b"old");
    }

    #[test]
    fn archive_rejects_traversal_and_symlinks() {
        assert!(!safe_archive_path(Path::new("../agent-talk")));
        assert!(safe_archive_path(Path::new("nested/agent-talk")));
        {
            let path = "agent-talk";
            let kind = tar::EntryType::Symlink;
            let workspace = tempfile::tempdir().unwrap();
            let archive_path = workspace.path().join("bad.tar.gz");
            let file = File::create(&archive_path).unwrap();
            let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(kind);
            header.set_size(if kind.is_file() { 3 } else { 0 });
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, path, Cursor::new(b"bin"))
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
            assert!(extract_binary(&archive_path, workspace.path()).is_err());
        }
    }

    #[test]
    fn atomic_replace_preserves_old_file_until_rename() {
        let workspace = tempfile::tempdir().unwrap();
        let target = workspace.path().join("agent-talk");
        let source = workspace.path().join("new");
        fs::write(&target, b"old").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&source, b"new").unwrap();

        atomic_replace(&target, &source).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
