//! Native update checks against this project's GitHub Releases.
//!
//! Downloads are HTTPS-only, size-bounded and must match the release asset's
//! SHA-256 digest (or a release `SHA256SUMS` entry) before they can run.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RELEASES_API: &str =
    "https://api.github.com/repos/Sandeep2062/token-monitor-lite/releases/latest";
const RELEASE_ASSET_PREFIX: &str =
    "https://github.com/Sandeep2062/token-monitor-lite/releases/download/";
const USER_AGENT: &str = concat!("TokenMonitorLite/", env!("CARGO_PKG_VERSION"));
const MAX_RELEASE_JSON_BYTES: u64 = 512 * 1024;
const MAX_SUMS_BYTES: u64 = 64 * 1024;
pub const MAX_UPDATE_BYTES: u64 = 300 * 1024 * 1024;
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdatePlatform {
    Windows,
    Linux,
    MacOs,
}

impl UpdatePlatform {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }

    fn asset_matches(self, name: &str) -> bool {
        name.starts_with("token-monitor-lite")
            && match self {
                Self::Windows => name.ends_with("-windows-x64-setup.exe"),
                Self::Linux => name.ends_with("_amd64.deb"),
                Self::MacOs => name.ends_with("-macos-arm64-unsigned.zip"),
            }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateAsset {
    pub name: String,
    pub url: String,
    pub size: u64,
    pub digest: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateCheck {
    pub version: String,
    pub asset: UpdateAsset,
    pub checksums: Option<UpdateAsset>,
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum UpdateError {
    #[error("update service is unreachable")]
    Network,
    #[error("no update release is published")]
    NotFound,
    #[error("release metadata is invalid")]
    InvalidRelease,
    #[error("release has no artifact for this platform")]
    NoAsset,
    #[error("release artifact has no SHA-256 digest")]
    DigestMissing,
    #[error("update checksum does not match")]
    DigestMismatch,
    #[error("update artifact exceeds the size limit")]
    TooLarge,
    #[error("downloaded artifact has an unexpected shape")]
    InvalidArtifact,
    #[error("downloaded artifact could not be installed")]
    Install,
    #[error("download storage failed")]
    Io,
}

/// Network boundary, isolated so release parsing and verification stay testable.
pub trait UpdateTransport: Send + Sync {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, UpdateError>;
    fn download(&self, url: &str, destination: &Path, limit: u64) -> Result<u64, UpdateError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HttpsTransport;

impl UpdateTransport for HttpsTransport {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, UpdateError> {
        if !url.starts_with("https://") {
            return Err(UpdateError::InvalidRelease);
        }
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(CHECK_TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
            .new_agent();
        let mut response = agent.get(url).call().map_err(transport_error)?;
        response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|_| UpdateError::Network)
    }

    fn download(&self, url: &str, destination: &Path, limit: u64) -> Result<u64, UpdateError> {
        if !url.starts_with("https://") {
            return Err(UpdateError::InvalidRelease);
        }
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(DOWNLOAD_TIMEOUT))
            .user_agent(USER_AGENT)
            .build()
            .new_agent();
        let response = agent.get(url).call().map_err(transport_error)?;
        let mut reader = response.into_body().into_reader().take(limit + 1);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(destination)
            .map_err(|_| UpdateError::Io)?;
        let bytes = io::copy(&mut reader, &mut file).map_err(|_| UpdateError::Io)?;
        file.flush().map_err(|_| UpdateError::Io)?;
        file.sync_all().map_err(|_| UpdateError::Io)?;
        if bytes > limit {
            return Err(UpdateError::TooLarge);
        }
        Ok(bytes)
    }
}

fn transport_error(error: ureq::Error) -> UpdateError {
    match error {
        ureq::Error::StatusCode(404) => UpdateError::NotFound,
        _ => UpdateError::Network,
    }
}

#[derive(Deserialize)]
struct ReleaseDocument {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<AssetDocument>,
}

#[derive(Deserialize)]
struct AssetDocument {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

/// Returns `None` when no newer stable release is published.
pub fn check_for_update(
    current: &str,
    platform: UpdatePlatform,
    transport: &dyn UpdateTransport,
) -> Result<Option<UpdateCheck>, UpdateError> {
    let current = parse_version(current).ok_or(UpdateError::InvalidRelease)?;
    let bytes = match transport.get(RELEASES_API, MAX_RELEASE_JSON_BYTES) {
        Ok(bytes) => bytes,
        Err(UpdateError::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let release: ReleaseDocument =
        serde_json::from_slice(&bytes).map_err(|_| UpdateError::InvalidRelease)?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = parse_version(&release.tag_name).ok_or(UpdateError::InvalidRelease)?;
    if version <= current || !version.pre.is_empty() {
        return Ok(None);
    }
    let mut matches = release
        .assets
        .iter()
        .filter(|asset| platform.asset_matches(&asset.name));
    let asset = matches.next().ok_or(UpdateError::NoAsset)?;
    if matches.next().is_some() {
        return Err(UpdateError::InvalidRelease);
    }
    validate_asset(asset)?;
    let checksums = release_checksums(&release.assets);
    Ok(Some(UpdateCheck {
        version: version.to_string(),
        asset: UpdateAsset {
            name: asset.name.clone(),
            url: asset.browser_download_url.clone(),
            size: asset.size,
            digest: asset.digest.clone(),
        },
        checksums,
    }))
}

fn release_checksums(assets: &[AssetDocument]) -> Option<UpdateAsset> {
    assets
        .iter()
        .find(|asset| matches!(asset.name.as_str(), "SHA256SUMS" | "sha256sums.txt"))
        .map(|asset| UpdateAsset {
            name: asset.name.clone(),
            url: asset.browser_download_url.clone(),
            size: asset.size,
            digest: asset.digest.clone(),
        })
        .filter(|asset| valid_asset_url(&asset.url))
}

fn validate_asset(asset: &AssetDocument) -> Result<(), UpdateError> {
    if !valid_asset_url(&asset.browser_download_url)
        || !safe_file_name(&asset.name)
        || asset.size > MAX_UPDATE_BYTES
    {
        return Err(UpdateError::InvalidRelease);
    }
    if let Some(digest) = asset.digest.as_deref()
        && digest_hash(digest).is_none()
    {
        return Err(UpdateError::DigestMissing);
    }
    Ok(())
}

fn valid_asset_url(url: &str) -> bool {
    url.starts_with(RELEASE_ASSET_PREFIX)
        && url.len() <= 2048
        && !url.contains(char::is_whitespace)
        && !url.contains("..")
}

fn safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn parse_version(value: &str) -> Option<semver::Version> {
    let value = value.trim().strip_prefix('v').unwrap_or(value.trim());
    semver::Version::parse(value).ok()
}

fn digest_hash(value: &str) -> Option<&str> {
    let hash = value.strip_prefix("sha256:")?;
    (hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(hash)
}

pub fn download_update(
    check: &UpdateCheck,
    root: &Path,
    transport: &dyn UpdateTransport,
) -> Result<PathBuf, UpdateError> {
    if !safe_file_name(&check.asset.name) || check.asset.size > MAX_UPDATE_BYTES {
        return Err(UpdateError::InvalidArtifact);
    }
    let directory = root.join("updates").join(&check.version);
    fs::create_dir_all(&directory).map_err(|_| UpdateError::Io)?;
    let temporary = directory.join(format!(".download-{}.tmp", unique_suffix()));
    let result = (|| {
        let bytes = transport.download(&check.asset.url, &temporary, MAX_UPDATE_BYTES)?;
        if bytes == 0 || (check.asset.size > 0 && bytes != check.asset.size) {
            return Err(UpdateError::InvalidArtifact);
        }
        let expected = match check.asset.digest.as_deref() {
            Some(digest) => digest_hash(digest)
                .ok_or(UpdateError::DigestMissing)?
                .to_owned(),
            None => checksum_from_sums(check, transport)?,
        };
        let actual = sha256_file(&temporary)?;
        if !actual.eq_ignore_ascii_case(&expected) {
            return Err(UpdateError::DigestMismatch);
        }
        let destination = directory.join(&check.asset.name);
        if destination.exists() {
            fs::remove_file(&destination).map_err(|_| UpdateError::Io)?;
        }
        fs::rename(&temporary, &destination).map_err(|_| UpdateError::Io)?;
        Ok(destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn checksum_from_sums(
    check: &UpdateCheck,
    transport: &dyn UpdateTransport,
) -> Result<String, UpdateError> {
    let sums = check.checksums.as_ref().ok_or(UpdateError::DigestMissing)?;
    if !valid_asset_url(&sums.url) {
        return Err(UpdateError::DigestMissing);
    }
    let bytes = transport.get(&sums.url, MAX_SUMS_BYTES)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| UpdateError::DigestMissing)?;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(hash) = fields.next() else {
            continue;
        };
        let Some(name) = fields.next() else {
            continue;
        };
        if name.trim_start_matches('*') == check.asset.name
            && hash.len() == 64
            && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Ok(hash.to_owned());
        }
    }
    Err(UpdateError::DigestMissing)
}

fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let mut file = File::open(path).map_err(|_| UpdateError::Io)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|_| UpdateError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(hex)
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:020}-{sequence:010}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Launched,
    Manual(PathBuf),
}

/// Launches a verified installer where the platform supports it. macOS bundles
/// are left for manual replacement until a tested bundle-swap helper exists.
pub fn install_update(
    path: &Path,
    platform: UpdatePlatform,
) -> Result<InstallOutcome, UpdateError> {
    if !safe_file_name(
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or(UpdateError::InvalidArtifact)?,
    ) {
        return Err(UpdateError::InvalidArtifact);
    }
    match platform {
        UpdatePlatform::Windows => {
            if path.extension().and_then(|value| value.to_str()) != Some("exe") {
                return Err(UpdateError::InvalidArtifact);
            }
            Command::new(path)
                .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
                .spawn()
                .map_err(|_| UpdateError::Install)?;
            Ok(InstallOutcome::Launched)
        }
        UpdatePlatform::Linux => {
            if path.extension().and_then(|value| value.to_str()) != Some("deb") {
                return Err(UpdateError::InvalidArtifact);
            }
            match Command::new("pkexec")
                .args(["dpkg", "-i"])
                .arg(path)
                .spawn()
            {
                Ok(_) => Ok(InstallOutcome::Launched),
                Err(_) => Ok(InstallOutcome::Manual(path.to_path_buf())),
            }
        }
        UpdatePlatform::MacOs => Ok(InstallOutcome::Manual(path.to_path_buf())),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InstallOutcome, UpdateAsset, UpdateCheck, UpdateError, UpdatePlatform, UpdateTransport,
        check_for_update, download_update, install_update,
    };
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct FakeTransport {
        files: BTreeMap<String, Vec<u8>>,
        downloads: Mutex<Vec<String>>,
    }

    impl FakeTransport {
        fn new(files: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
            Self {
                files: files.into_iter().collect(),
                downloads: Mutex::new(Vec::new()),
            }
        }
    }

    impl UpdateTransport for FakeTransport {
        fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, UpdateError> {
            self.files
                .get(url)
                .cloned()
                .filter(|bytes| bytes.len() as u64 <= limit)
                .ok_or(UpdateError::Network)
        }

        fn download(&self, url: &str, destination: &Path, limit: u64) -> Result<u64, UpdateError> {
            self.downloads
                .lock()
                .expect("download log")
                .push(url.to_owned());
            let bytes = self
                .files
                .get(url)
                .cloned()
                .filter(|bytes| bytes.len() as u64 <= limit)
                .ok_or(UpdateError::Network)?;
            std::fs::write(destination, &bytes).map_err(|_| UpdateError::Io)?;
            Ok(bytes.len() as u64)
        }
    }

    fn payload() -> Vec<u8> {
        b"verified installer bytes".to_vec()
    }

    fn digest(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let mut hex = String::new();
        for byte in hasher.finalize() {
            use std::fmt::Write as _;
            write!(&mut hex, "{byte:02x}").expect("hex");
        }
        hex
    }

    fn release_json_full(version: &str, assets: &str) -> String {
        format!(
            r#"{{"tag_name":"v{version}","draft":false,"prerelease":false,"assets":[{assets}]}}"#
        )
    }

    fn asset_json(name: &str, version: &str, size: usize, digest: Option<&str>) -> String {
        let digest = digest
            .map(|value| format!(r#","digest":"sha256:{value}""#))
            .unwrap_or_default();
        format!(
            r#"{{"name":"{name}","browser_download_url":"https://github.com/Sandeep2062/token-monitor-lite/releases/download/v{version}/{name}","size":{size}{digest}}}"#
        )
    }

    fn transport_with_release(json: String) -> FakeTransport {
        FakeTransport::new([(super::RELEASES_API.to_owned(), json.into_bytes())])
    }

    #[test]
    fn newer_stable_release_selects_the_platform_asset() {
        let bytes = payload();
        let asset = "token-monitor-lite-0.2.0-windows-x64-setup.exe";
        let json = release_json_full(
            "0.2.0",
            &asset_json(asset, "0.2.0", bytes.len(), Some(&digest(&bytes))),
        );
        let transport = transport_with_release(json);
        let check = check_for_update("0.1.0", UpdatePlatform::Windows, &transport)
            .expect("check")
            .expect("update");
        assert_eq!(check.version, "0.2.0");
        assert_eq!(check.asset.name, asset);
        assert_eq!(
            check.asset.digest,
            Some(format!("sha256:{}", digest(&bytes)))
        );
    }

    #[test]
    fn current_draft_prerelease_and_prerelease_version_are_not_updates() {
        let bytes = payload();
        let asset = asset_json(
            "token-monitor-lite-0.1.0-windows-x64-setup.exe",
            "0.1.0",
            bytes.len(),
            None,
        );
        let transport = transport_with_release(release_json_full("0.1.0", &asset));
        assert_eq!(
            check_for_update("0.1.0", UpdatePlatform::Windows, &transport),
            Ok(None)
        );
        let draft =
            release_json_full("0.2.0", &asset).replace(r#""draft":false"#, r#""draft":true"#);
        assert_eq!(
            check_for_update(
                "0.1.0",
                UpdatePlatform::Windows,
                &transport_with_release(draft)
            ),
            Ok(None)
        );
        let prerelease = release_json_full("0.2.0", &asset)
            .replace(r#""prerelease":false"#, r#""prerelease":true"#);
        assert_eq!(
            check_for_update(
                "0.1.0",
                UpdatePlatform::Windows,
                &transport_with_release(prerelease)
            ),
            Ok(None)
        );
        assert_eq!(
            check_for_update(
                "0.1.0",
                UpdatePlatform::Windows,
                &transport_with_release(release_json_full("0.2.0-beta.1", &asset))
            ),
            Ok(None)
        );
    }

    #[test]
    fn missing_or_insecure_assets_are_rejected() {
        let bytes = payload();
        let other = asset_json("other.zip", "0.2.0", bytes.len(), None);
        assert_eq!(
            check_for_update(
                "0.1.0",
                UpdatePlatform::Windows,
                &transport_with_release(release_json_full("0.2.0", &other))
            ),
            Err(UpdateError::NoAsset)
        );
        let insecure = asset_json(
            "token-monitor-lite-0.2.0-windows-x64-setup.exe",
            "0.2.0",
            bytes.len(),
            None,
        )
        .replace(
            "https://github.com/Sandeep2062/token-monitor-lite/releases/download/",
            "https://evil.example/",
        );
        assert_eq!(
            check_for_update(
                "0.1.0",
                UpdatePlatform::Windows,
                &transport_with_release(release_json_full("0.2.0", &insecure))
            ),
            Err(UpdateError::InvalidRelease)
        );
    }

    #[test]
    fn download_verifies_the_asset_digest() {
        let bytes = payload();
        let asset = "token-monitor-lite-0.2.0-windows-x64-setup.exe";
        let url = format!(
            "https://github.com/Sandeep2062/token-monitor-lite/releases/download/v0.2.0/{asset}"
        );
        let check = UpdateCheck {
            version: "0.2.0".to_owned(),
            asset: UpdateAsset {
                name: asset.to_owned(),
                url: url.clone(),
                size: bytes.len() as u64,
                digest: Some(format!("sha256:{}", digest(&bytes))),
            },
            checksums: None,
        };
        let root = std::env::temp_dir().join(format!(
            "token-monitor-update-test-{}-{}",
            std::process::id(),
            super::unique_suffix()
        ));
        let transport = FakeTransport::new([(url, bytes.clone())]);
        let path = download_update(&check, &root, &transport).expect("download");
        assert_eq!(std::fs::read(&path).expect("read"), bytes);
        assert!(path.ends_with(asset));
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn download_rejects_a_digest_mismatch_and_leaves_no_artifact() {
        let bytes = payload();
        let asset = "token-monitor-lite-0.2.0-windows-x64-setup.exe";
        let url = format!(
            "https://github.com/Sandeep2062/token-monitor-lite/releases/download/v0.2.0/{asset}"
        );
        let check = UpdateCheck {
            version: "0.2.0".to_owned(),
            asset: UpdateAsset {
                name: asset.to_owned(),
                url: url.clone(),
                size: bytes.len() as u64,
                digest: Some(format!("sha256:{}", digest(b"different"))),
            },
            checksums: None,
        };
        let root = std::env::temp_dir().join(format!(
            "token-monitor-update-test-{}-{}",
            std::process::id(),
            super::unique_suffix()
        ));
        let transport = FakeTransport::new([(url, bytes)]);
        assert_eq!(
            download_update(&check, &root, &transport),
            Err(UpdateError::DigestMismatch)
        );
        let directory = root.join("updates").join("0.2.0");
        assert!(!directory.join(asset).exists());
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn sums_file_is_a_verified_fallback() {
        let bytes = payload();
        let asset = "token-monitor-lite-0.2.0-windows-x64-setup.exe";
        let version = "0.2.0";
        let base = format!(
            "https://github.com/Sandeep2062/token-monitor-lite/releases/download/v{version}"
        );
        let sums_url = format!("{base}/SHA256SUMS");
        let check = UpdateCheck {
            version: version.to_owned(),
            asset: UpdateAsset {
                name: asset.to_owned(),
                url: format!("{base}/{asset}"),
                size: bytes.len() as u64,
                digest: None,
            },
            checksums: Some(UpdateAsset {
                name: "SHA256SUMS".to_owned(),
                url: sums_url.clone(),
                size: 0,
                digest: None,
            }),
        };
        let sums = format!("{}  {asset}\n", digest(&bytes));
        let root = std::env::temp_dir().join(format!(
            "token-monitor-update-test-{}-{}",
            std::process::id(),
            super::unique_suffix()
        ));
        let transport = FakeTransport::new([
            (sums_url, sums.into_bytes()),
            (check.asset.url.clone(), bytes),
        ]);
        let path = download_update(&check, &root, &transport).expect("download");
        assert!(path.exists());
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn install_rejects_unexpected_artifacts_without_executing_them() {
        let path = std::env::temp_dir().join(format!(
            "token-monitor-update-install-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, b"not an installer").expect("write");
        assert_eq!(
            install_update(&path, UpdatePlatform::Windows),
            Err(UpdateError::InvalidArtifact)
        );
        std::fs::remove_file(path).expect("cleanup");
        assert_eq!(
            install_update(
                &std::env::temp_dir()
                    .join(format!("missing-update-{}.exe", super::unique_suffix())),
                UpdatePlatform::Windows
            ),
            Err(UpdateError::Install)
        );
        assert!(matches!(
            install_update(&PathBuf::from("update.zip"), UpdatePlatform::MacOs),
            Ok(InstallOutcome::Manual(_))
        ));
    }
}
