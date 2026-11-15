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
use std::sync::OnceLock;
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
const MAX_REDIRECTS: usize = 5;
const DIGEST_SUFFIX: &str = ".sha256";

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

/// One pooled client for every request. ureq's own redirect follower is
/// disabled (`max_redirects(0)`) so each hop can be checked against
/// [`allowed_update_url`] before it is requested.
fn shared_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .https_only(true)
            .max_redirects(0)
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build()
            .new_agent()
    })
}

impl UpdateTransport for HttpsTransport {
    fn get(&self, url: &str, limit: u64) -> Result<Vec<u8>, UpdateError> {
        let mut response = http_get(url, CHECK_TIMEOUT)?;
        response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|_| UpdateError::Network)
    }

    fn download(&self, url: &str, destination: &Path, limit: u64) -> Result<u64, UpdateError> {
        let response = http_get(url, DOWNLOAD_TIMEOUT)?;
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

fn http_get(url: &str, timeout: Duration) -> Result<ureq::http::Response<ureq::Body>, UpdateError> {
    let mut target = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        if !allowed_update_url(&target) {
            return Err(UpdateError::InvalidRelease);
        }
        let response = shared_agent()
            .get(target.as_str())
            .config()
            .timeout_global(Some(timeout))
            .build()
            .call()
            .map_err(transport_error)?;
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .ok_or(UpdateError::Network)?;
            target = redirect_target(location)?;
            continue;
        }
        if status.as_u16() == 404 {
            return Err(UpdateError::NotFound);
        }
        if !status.is_success() {
            return Err(UpdateError::Network);
        }
        return Ok(response);
    }
    Err(UpdateError::Network)
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

/// Only GitHub release infrastructure may be contacted, and only over HTTPS.
/// GitHub serves release assets from a `githubusercontent.com` host after a
/// redirect: `objects.githubusercontent.com` historically and
/// `release-assets.githubusercontent.com` currently. Both, plus `github.com`
/// and its GitHub-owned subdomains such as `api.github.com`, are allowed.
fn allowed_update_url(url: &str) -> bool {
    if url.len() > 2048 || url.contains(char::is_whitespace) || url.contains("..") {
        return false;
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || authority.starts_with('[') {
        return false;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port))
            if !host.contains(':')
                && !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            (host, Some(port))
        }
        Some(_) => return false,
        None => (authority, None),
    };
    if port.is_some_and(|port| port != "443") {
        return false;
    }
    if host.is_empty() || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    if !host
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
    {
        return false;
    }
    let host = host.to_ascii_lowercase();
    host == "github.com"
        || host.ends_with(".github.com")
        || host == "objects.githubusercontent.com"
        || host == "release-assets.githubusercontent.com"
}

fn redirect_target(location: &str) -> Result<String, UpdateError> {
    if allowed_update_url(location) {
        Ok(location.to_owned())
    } else {
        Err(UpdateError::InvalidRelease)
    }
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
    let directory = ensure_updates_root(root)?.join(&check.version);
    fs::create_dir_all(&directory).map_err(|_| UpdateError::Io)?;
    let temporary = directory.join(format!(".download-{}.tmp", unique_suffix()));
    let mut placed = None;
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
        let expected = expected.to_ascii_lowercase();
        let actual = sha256_file(&temporary)?;
        if !actual.eq_ignore_ascii_case(&expected) {
            return Err(UpdateError::DigestMismatch);
        }
        let destination = directory.join(&check.asset.name);
        if destination.exists() {
            fs::remove_file(&destination).map_err(|_| UpdateError::Io)?;
        }
        fs::rename(&temporary, &destination).map_err(|_| UpdateError::Io)?;
        placed = Some(destination.clone());
        write_verified_digest(&destination, &expected)?;
        Ok(destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        if let Some(destination) = placed {
            let _ = fs::remove_file(&destination);
            if let Ok(sidecar) = digest_sidecar(&destination) {
                let _ = fs::remove_file(sidecar);
            }
        }
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

fn digest_sidecar(path: &Path) -> Result<PathBuf, UpdateError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(UpdateError::InvalidArtifact)?;
    Ok(path.with_file_name(format!("{name}{DIGEST_SUFFIX}")))
}

/// Records the digest that was checked against the release metadata so that
/// installation can re-verify the bytes it is about to execute.
fn write_verified_digest(path: &Path, digest: &str) -> Result<(), UpdateError> {
    fs::write(digest_sidecar(path)?, format!("{digest}\n")).map_err(|_| UpdateError::Io)
}

fn read_verified_digest(path: &Path) -> Result<String, UpdateError> {
    let sidecar = digest_sidecar(path)?;
    let text = fs::read_to_string(&sidecar).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            UpdateError::DigestMissing
        } else {
            UpdateError::Io
        }
    })?;
    let digest = text.trim();
    (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| digest.to_ascii_lowercase())
        .ok_or(UpdateError::DigestMissing)
}

fn verify_artifact_digest(path: &Path, expected: &str) -> Result<(), UpdateError> {
    if sha256_file(path)?.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(UpdateError::DigestMismatch)
    }
}

fn ensure_updates_root(root: &Path) -> Result<PathBuf, UpdateError> {
    let updates = root.join("updates");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&updates)
            .map_err(|_| UpdateError::Io)?;
        fs::set_permissions(&updates, fs::Permissions::from_mode(0o700))
            .map_err(|_| UpdateError::Io)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(&updates).map_err(|_| UpdateError::Io)?;
    }
    #[cfg(windows)]
    {
        let _ = restrict_windows_acl(&updates);
    }
    Ok(updates)
}

#[cfg(windows)]
fn restrict_windows_acl(directory: &Path) -> bool {
    use std::process::Stdio;
    let program = std::env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join("icacls.exe"))
        .unwrap_or_else(|| PathBuf::from("icacls"));
    let Some(user) = std::env::var("USERNAME")
        .ok()
        .filter(|user| !user.is_empty())
    else {
        return false;
    };
    Command::new(program)
        .arg(directory)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:(OI)(CI)F"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Copies the downloaded package into a fresh 0700 directory and re-hashes the
/// copy, so the bytes handed to `pkexec` cannot be swapped after verification.
#[cfg(unix)]
fn stage_linux_installer(path: &Path, expected: &str) -> Result<PathBuf, UpdateError> {
    use std::os::unix::fs::DirBuilderExt;
    let parent = path.parent().ok_or(UpdateError::InvalidArtifact)?;
    let name = path.file_name().ok_or(UpdateError::InvalidArtifact)?;
    let staging = parent.join(format!(".install-{}", unique_suffix()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(|_| UpdateError::Io)?;
    let copy = staging.join(name);
    let staged = (|| {
        fs::copy(path, &copy).map_err(|_| UpdateError::Io)?;
        verify_artifact_digest(&copy, expected)
    })();
    if let Err(error) = staged {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    Ok(copy)
}

#[cfg(not(unix))]
fn stage_linux_installer(_path: &Path, _expected: &str) -> Result<PathBuf, UpdateError> {
    Err(UpdateError::Install)
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

/// Launches a verified installer where the platform supports it. The artifact
/// is re-hashed against the digest recorded at download time immediately before
/// execution. macOS bundles are left for manual replacement until a tested
/// bundle-swap helper exists.
pub fn install_update(
    path: &Path,
    platform: UpdatePlatform,
) -> Result<InstallOutcome, UpdateError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(UpdateError::InvalidArtifact)?;
    if !safe_file_name(name) {
        return Err(UpdateError::InvalidArtifact);
    }
    let extension = path.extension().and_then(|value| value.to_str());
    match platform {
        UpdatePlatform::Windows if extension != Some("exe") => {
            return Err(UpdateError::InvalidArtifact);
        }
        UpdatePlatform::Linux if extension != Some("deb") => {
            return Err(UpdateError::InvalidArtifact);
        }
        _ => {}
    }
    let expected = read_verified_digest(path)?;
    match platform {
        UpdatePlatform::Windows => {
            verify_artifact_digest(path, &expected)?;
            Command::new(path)
                .args(["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
                .spawn()
                .map_err(|_| UpdateError::Install)?;
            Ok(InstallOutcome::Launched)
        }
        UpdatePlatform::Linux => {
            let staged = stage_linux_installer(path, &expected)?;
            match Command::new("pkexec")
                .args(["dpkg", "-i"])
                .arg(&staged)
                .spawn()
            {
                Ok(_) => Ok(InstallOutcome::Launched),
                Err(_) => Ok(InstallOutcome::Manual(path.to_path_buf())),
            }
        }
        UpdatePlatform::MacOs => {
            verify_artifact_digest(path, &expected)?;
            Ok(InstallOutcome::Manual(path.to_path_buf()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        InstallOutcome, UpdateAsset, UpdateCheck, UpdateError, UpdatePlatform, UpdateTransport,
        allowed_update_url, check_for_update, download_update, ensure_updates_root, install_update,
        redirect_target, shared_agent,
    };
    #[cfg(unix)]
    use super::{stage_linux_installer, write_verified_digest};
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

    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "token-monitor-update-{label}-{}-{}",
            std::process::id(),
            super::unique_suffix()
        ))
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
        let root = temp_root("download-digest");
        let transport = FakeTransport::new([(url, bytes.clone())]);
        let path = download_update(&check, &root, &transport).expect("download");
        assert_eq!(std::fs::read(&path).expect("read"), bytes);
        assert!(path.ends_with(asset));
        assert_eq!(
            std::fs::read_to_string(path.with_file_name(format!("{asset}.sha256")))
                .expect("sidecar")
                .trim(),
            digest(&bytes)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(root.join("updates"))
                    .expect("updates")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
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
        let root = temp_root("download-mismatch");
        let transport = FakeTransport::new([(url, bytes)]);
        assert_eq!(
            download_update(&check, &root, &transport),
            Err(UpdateError::DigestMismatch)
        );
        let directory = root.join("updates").join("0.2.0");
        assert!(!directory.join(asset).exists());
        assert!(!directory.join(format!("{asset}.sha256")).exists());
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
        let root = temp_root("download-sums");
        let transport = FakeTransport::new([
            (sums_url, sums.into_bytes()),
            (check.asset.url.clone(), bytes.clone()),
        ]);
        let path = download_update(&check, &root, &transport).expect("download");
        assert!(path.exists());
        assert_eq!(
            std::fs::read_to_string(path.with_file_name(format!("{asset}.sha256")))
                .expect("sidecar")
                .trim(),
            digest(&bytes)
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn install_rejects_unexpected_artifacts_without_executing_them() {
        let root = temp_root("install-shape");
        std::fs::create_dir_all(&root).expect("mkdir");
        let path = root.join("installer.txt");
        std::fs::write(&path, b"not an installer").expect("write");
        assert_eq!(
            install_update(&path, UpdatePlatform::Windows),
            Err(UpdateError::InvalidArtifact)
        );
        assert_eq!(
            install_update(&path, UpdatePlatform::Linux),
            Err(UpdateError::InvalidArtifact)
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
        assert_eq!(
            install_update(
                &temp_root("install-missing").join("missing.exe"),
                UpdatePlatform::Windows
            ),
            Err(UpdateError::DigestMissing)
        );
    }

    #[test]
    fn transport_accepts_only_github_hosts_over_https() {
        for url in [
            "https://github.com/Sandeep2062/token-monitor-lite/releases/download/v0.2.0/a.deb",
            "https://api.github.com/repos/Sandeep2062/token-monitor-lite/releases/latest",
            "https://objects.githubusercontent.com/github-production-release-asset/1?x=2",
            "https://release-assets.githubusercontent.com/github-production-release-asset/1?x=2",
            "https://github.com:443/releases",
        ] {
            assert!(allowed_update_url(url), "{url}");
        }
        for url in [
            "http://github.com/x",
            "https://evil.example/x",
            "https://github.com.evil.example/x",
            "https://evilgithub.com/x",
            "https://user@github.com/x",
            "https://github.com:8443/x",
            "https://github.com:/x",
            "https://github.com./x",
            "ftp://github.com/x",
            "/relative",
            "//github.com/x",
            "https://raw.githubusercontent.com/x",
        ] {
            assert!(!allowed_update_url(url), "{url}");
        }
    }

    #[test]
    fn redirects_must_target_an_allowed_https_host() {
        let asset =
            "https://release-assets.githubusercontent.com/github-production-release-asset/1";
        assert_eq!(redirect_target(asset), Ok(asset.to_owned()));
        for location in [
            "http://release-assets.githubusercontent.com/asset",
            "https://evil.example/asset",
            "/relative",
            "//objects.githubusercontent.com/asset",
        ] {
            assert_eq!(
                redirect_target(location),
                Err(UpdateError::InvalidRelease),
                "{location}"
            );
        }
    }

    #[test]
    fn https_transport_rejects_disallowed_urls_before_connecting() {
        let root = temp_root("transport");
        std::fs::create_dir_all(&root).expect("mkdir");
        let destination = root.join("artifact.bin");
        assert_eq!(
            super::HttpsTransport.get("https://evil.example/release", 1024),
            Err(UpdateError::InvalidRelease)
        );
        assert_eq!(
            super::HttpsTransport.download("http://github.com/release", &destination, 1024),
            Err(UpdateError::InvalidRelease)
        );
        assert!(!destination.exists());
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn one_locked_down_agent_is_reused() {
        let agent = shared_agent();
        assert!(std::ptr::eq(agent, shared_agent()));
        assert_eq!(agent.config().max_redirects(), 0);
        assert!(agent.config().https_only());
        assert!(!agent.config().http_status_as_error());
    }

    #[cfg(unix)]
    #[test]
    fn updates_directory_is_tightened_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("private-root");
        let updates = root.join("updates");
        std::fs::create_dir_all(&updates).expect("mkdir");
        std::fs::set_permissions(&updates, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let secured = ensure_updates_root(&root).expect("updates");
        assert_eq!(secured, updates);
        assert_eq!(
            std::fs::metadata(&updates)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn install_rehashes_before_launching() {
        let root = temp_root("rehash");
        let directory = ensure_updates_root(&root).expect("updates").join("0.2.0");
        std::fs::create_dir_all(&directory).expect("mkdir");
        let path = directory.join("token-monitor-lite-0.2.0-amd64.deb");
        std::fs::write(&path, b"verified package").expect("write");
        write_verified_digest(&path, &digest(b"verified package")).expect("digest");
        std::fs::write(&path, b"swapped package!").expect("swap");
        assert_eq!(
            install_update(&path, UpdatePlatform::Linux),
            Err(UpdateError::DigestMismatch)
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn linux_package_is_copied_into_a_fresh_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("stage");
        let directory = ensure_updates_root(&root).expect("updates").join("0.2.0");
        std::fs::create_dir_all(&directory).expect("mkdir");
        let path = directory.join("token-monitor-lite-0.2.0-amd64.deb");
        std::fs::write(&path, b"package bytes").expect("write");
        let staged = stage_linux_installer(&path, &digest(b"package bytes")).expect("stage");
        assert_ne!(staged, path);
        assert_eq!(std::fs::read(&staged).expect("read"), b"package bytes");
        let staging = staged.parent().expect("parent");
        assert!(
            staging
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".install-"))
        );
        assert_eq!(
            std::fs::metadata(staging)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(path.exists());
        assert_eq!(
            stage_linux_installer(&path, &digest(b"different")),
            Err(UpdateError::DigestMismatch)
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[test]
    fn install_requires_the_recorded_digest() {
        let root = temp_root("install-digest");
        std::fs::create_dir_all(&root).expect("mkdir");
        let path = root.join("token-monitor-lite-0.2.0-macos-arm64-unsigned.zip");
        std::fs::write(&path, b"bundle bytes").expect("write");
        let sidecar =
            path.with_file_name("token-monitor-lite-0.2.0-macos-arm64-unsigned.zip.sha256");
        assert_eq!(
            install_update(&path, UpdatePlatform::MacOs),
            Err(UpdateError::DigestMissing)
        );
        std::fs::write(&sidecar, b"not-a-digest\n").expect("write");
        assert_eq!(
            install_update(&path, UpdatePlatform::MacOs),
            Err(UpdateError::DigestMissing)
        );
        std::fs::write(&sidecar, format!("{}\n", digest(b"different"))).expect("write");
        assert_eq!(
            install_update(&path, UpdatePlatform::MacOs),
            Err(UpdateError::DigestMismatch)
        );
        std::fs::write(&sidecar, format!("{}\n", digest(b"bundle bytes"))).expect("write");
        assert_eq!(
            install_update(&path, UpdatePlatform::MacOs),
            Ok(InstallOutcome::Manual(path.clone()))
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }

    #[cfg(windows)]
    #[test]
    fn updates_directory_acl_is_restricted_on_windows() {
        let root = temp_root("acl");
        let updates = ensure_updates_root(&root).expect("updates");
        assert!(updates.is_dir());
        assert!(super::restrict_windows_acl(&updates), "icacls must succeed");
        let user = std::env::var("USERNAME").expect("username");
        let output = std::process::Command::new("icacls")
            .arg(&updates)
            .output()
            .expect("icacls output");
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .to_lowercase()
                .contains(&user.to_lowercase())
        );
        std::fs::remove_dir_all(&root).expect("cleanup");
    }
}
