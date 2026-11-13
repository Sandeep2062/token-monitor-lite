//! Platform credential storage and secret-bearing migration planning.
//!
//! Secret values stay inside this backend module. They are sent through one
//! bounded worker and never appear in debug output, SQLite, or UI models.

use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use thiserror::Error;

const SERVICE_NAME: &str = "token-monitor-lite";
const COMMAND_CAPACITY: usize = 4;
const CALL_DEADLINE: Duration = Duration::from_secs(2);
const MAX_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialKind {
    Fixed,
    ClaudeAccount,
    MimoAccount,
    AntigravityAccount,
    WorkbuddyAccount,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::ClaudeAccount => "claude-account",
            Self::MimoAccount => "mimo-account",
            Self::AntigravityAccount => "antigravity-account",
            Self::WorkbuddyAccount => "workbuddy-account",
        }
    }
}

pub struct CredentialMaterial(SecretBytes);

impl CredentialMaterial {
    pub fn from_secret_bytes(bytes: Vec<u8>) -> Result<Self, CredentialError> {
        if bytes.is_empty() {
            return Err(CredentialError::InvalidInput);
        }
        Ok(Self(SecretBytes::new(bytes)))
    }

    fn into_secret(self) -> Result<SecretBytes, CredentialError> {
        Ok(self.0)
    }
}

impl fmt::Debug for CredentialMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialMaterial([redacted])")
    }
}

#[derive(PartialEq, Eq)]
pub struct SecretBytes(Vec<u8>);

impl SecretBytes {
    fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    pub fn matches(&self, expected: &SecretBytes) -> bool {
        if self.0.len() != expected.0.len() {
            return false;
        }
        self.0
            .iter()
            .zip(&expected.0)
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

impl fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretBytes([redacted])")
    }
}

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub struct CredentialTransfer {
    pub credential_id: String,
    pub kind: CredentialKind,
    material: CredentialMaterial,
}

impl CredentialTransfer {
    pub fn into_material(self) -> CredentialMaterial {
        self.material
    }
}

impl fmt::Debug for CredentialTransfer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialTransfer")
            .field("credential_id", &self.credential_id)
            .field("kind", &self.kind)
            .field("material", &"[redacted]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialPlanIssueKind {
    UnsupportedDocument,
    UnknownBranch,
    InvalidSupportedBranch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialPlanIssue {
    pub branch_id: String,
    pub kind: CredentialPlanIssueKind,
}

#[derive(Debug)]
pub struct CredentialImportPlan {
    pub transfers: Vec<CredentialTransfer>,
    pub issues: Vec<CredentialPlanIssue>,
    pub stripped_settings: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum CredentialError {
    #[error("credential input is invalid")]
    InvalidInput,
    #[error("platform credential storage is unavailable")]
    Unavailable,
    #[error("credential was not found")]
    NotFound,
    #[error("credential request is stale")]
    StaleGeneration,
    #[error("credential request queue is busy")]
    Busy,
    #[error("credential request timed out")]
    TimedOut,
    #[error("credential worker is closed")]
    Closed,
    #[error("credential worker failed")]
    WorkerPanicked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackendError {
    Unavailable,
    NotFound,
    Failed,
}

trait CredentialBackend: Send + 'static {
    fn set(&mut self, username: &str, secret: &[u8]) -> Result<(), BackendError>;
    fn get(&mut self, username: &str) -> Result<Vec<u8>, BackendError>;
    fn delete(&mut self, username: &str) -> Result<(), BackendError>;
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct TestCredentialStore {
    inner: Arc<Mutex<TestCredentialStoreState>>,
}

#[cfg(test)]
#[derive(Default)]
struct TestCredentialStoreState {
    values: std::collections::BTreeMap<String, Vec<u8>>,
    unavailable: bool,
    fail_next_get: bool,
}

#[cfg(test)]
impl TestCredentialStore {
    pub(crate) fn set_unavailable(&self, unavailable: bool) {
        if let Ok(mut state) = self.inner.lock() {
            state.unavailable = unavailable;
        }
    }

    pub(crate) fn fail_next_get(&self) {
        if let Ok(mut state) = self.inner.lock() {
            state.fail_next_get = true;
        }
    }
}

#[cfg(test)]
struct TestCredentialBackend(TestCredentialStore);

#[cfg(test)]
impl CredentialBackend for TestCredentialBackend {
    fn set(&mut self, username: &str, secret: &[u8]) -> Result<(), BackendError> {
        let mut state = self.0.inner.lock().map_err(|_| BackendError::Failed)?;
        if state.unavailable {
            return Err(BackendError::Unavailable);
        }
        state.values.insert(username.to_owned(), secret.to_vec());
        Ok(())
    }

    fn get(&mut self, username: &str) -> Result<Vec<u8>, BackendError> {
        let mut state = self.0.inner.lock().map_err(|_| BackendError::Failed)?;
        if state.unavailable || state.fail_next_get {
            state.fail_next_get = false;
            return Err(BackendError::Unavailable);
        }
        state
            .values
            .get(username)
            .cloned()
            .ok_or(BackendError::NotFound)
    }

    fn delete(&mut self, username: &str) -> Result<(), BackendError> {
        let mut state = self.0.inner.lock().map_err(|_| BackendError::Failed)?;
        if state.unavailable {
            return Err(BackendError::Unavailable);
        }
        state
            .values
            .remove(username)
            .ok_or(BackendError::NotFound)?;
        Ok(())
    }
}

struct KeyringBackend;

impl KeyringBackend {
    fn entry(username: &str) -> Result<keyring::Entry, BackendError> {
        keyring::Entry::new(SERVICE_NAME, username).map_err(map_keyring_error)
    }
}

impl CredentialBackend for KeyringBackend {
    fn set(&mut self, username: &str, secret: &[u8]) -> Result<(), BackendError> {
        Self::entry(username)?
            .set_secret(secret)
            .map_err(map_keyring_error)
    }

    fn get(&mut self, username: &str) -> Result<Vec<u8>, BackendError> {
        Self::entry(username)?
            .get_secret()
            .map_err(map_keyring_error)
    }

    fn delete(&mut self, username: &str) -> Result<(), BackendError> {
        Self::entry(username)?
            .delete_credential()
            .map_err(map_keyring_error)
    }
}

fn map_keyring_error(error: keyring::Error) -> BackendError {
    match error {
        keyring::Error::NoEntry => BackendError::NotFound,
        keyring::Error::PlatformFailure(_)
        | keyring::Error::NoStorageAccess(_)
        | keyring::Error::Ambiguous(_) => BackendError::Unavailable,
        _ => BackendError::Failed,
    }
}

enum Command {
    Set {
        generation: u64,
        credential_id: String,
        material: CredentialMaterial,
        response: SyncSender<Result<SecretBytes, CredentialError>>,
    },
    Get {
        generation: u64,
        credential_id: String,
        response: SyncSender<Result<SecretBytes, CredentialError>>,
    },
    Verify {
        generation: u64,
        credential_id: String,
        material: CredentialMaterial,
        response: SyncSender<Result<SecretBytes, CredentialError>>,
    },
    Delete {
        generation: u64,
        credential_id: String,
        response: SyncSender<Result<SecretBytes, CredentialError>>,
    },
    Shutdown,
}

struct CredentialInner {
    sender: SyncSender<Command>,
    stopped: Mutex<bool>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl CredentialInner {
    fn stop_and_join(&self) -> Result<(), CredentialError> {
        {
            let mut stopped = self.stopped.lock().map_err(|_| CredentialError::Closed)?;
            if !*stopped {
                *stopped = true;
                self.sender
                    .send(Command::Shutdown)
                    .map_err(|_| CredentialError::Closed)?;
            }
        }
        if let Some(join) = self
            .join
            .lock()
            .map_err(|_| CredentialError::Closed)?
            .take()
        {
            join.join().map_err(|_| CredentialError::WorkerPanicked)?;
        }
        Ok(())
    }
}

impl Drop for CredentialInner {
    fn drop(&mut self) {
        if let Ok(stopped) = self.stopped.get_mut()
            && !*stopped
        {
            *stopped = true;
            let _ = self.sender.send(Command::Shutdown);
        }
        if let Ok(join) = self.join.get_mut()
            && let Some(join) = join.take()
        {
            let _ = join.join();
        }
    }
}

#[derive(Clone)]
pub struct CredentialService {
    inner: Arc<CredentialInner>,
}

impl fmt::Debug for CredentialService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialService { backend: [redacted] }")
    }
}

impl CredentialService {
    pub fn start() -> Result<Self, CredentialError> {
        Self::start_backend(KeyringBackend)
    }

    #[cfg(test)]
    fn start_with_backend(backend: impl CredentialBackend) -> Result<Self, CredentialError> {
        Self::start_backend(backend)
    }

    #[cfg(test)]
    pub(crate) fn start_test(store: TestCredentialStore) -> Result<Self, CredentialError> {
        Self::start_backend(TestCredentialBackend(store))
    }

    fn start_backend(backend: impl CredentialBackend) -> Result<Self, CredentialError> {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let join = thread::Builder::new()
            .name("token-monitor-credentials".to_owned())
            .spawn(move || worker_main(receiver, backend))
            .map_err(|_| CredentialError::Unavailable)?;
        Ok(Self {
            inner: Arc::new(CredentialInner {
                sender,
                stopped: Mutex::new(false),
                join: Mutex::new(Some(join)),
            }),
        })
    }

    pub fn set(
        &self,
        generation: u64,
        credential_id: &str,
        material: CredentialMaterial,
    ) -> Result<SecretBytes, CredentialError> {
        let credential_id = validate_credential_id(credential_id)?.to_owned();
        self.submit(|response| Command::Set {
            generation,
            credential_id,
            material,
            response,
        })
    }

    pub fn get(
        &self,
        generation: u64,
        credential_id: &str,
    ) -> Result<SecretBytes, CredentialError> {
        let credential_id = validate_credential_id(credential_id)?.to_owned();
        self.submit(|response| Command::Get {
            generation,
            credential_id,
            response,
        })
    }

    pub fn verify(
        &self,
        generation: u64,
        credential_id: &str,
        material: CredentialMaterial,
    ) -> Result<(), CredentialError> {
        let credential_id = validate_credential_id(credential_id)?.to_owned();
        self.submit(|response| Command::Verify {
            generation,
            credential_id,
            material,
            response,
        })
        .map(drop)
    }

    pub fn delete(&self, generation: u64, credential_id: &str) -> Result<(), CredentialError> {
        let credential_id = validate_credential_id(credential_id)?.to_owned();
        self.submit(|response| Command::Delete {
            generation,
            credential_id,
            response,
        })
        .map(drop)
    }

    fn submit(
        &self,
        make_command: impl FnOnce(SyncSender<Result<SecretBytes, CredentialError>>) -> Command,
    ) -> Result<SecretBytes, CredentialError> {
        let (response_sender, response_receiver) = mpsc::sync_channel(1);
        {
            let stopped = self
                .inner
                .stopped
                .lock()
                .map_err(|_| CredentialError::Closed)?;
            if *stopped {
                return Err(CredentialError::Closed);
            }
            self.inner
                .sender
                .try_send(make_command(response_sender))
                .map_err(|error| match error {
                    mpsc::TrySendError::Full(_) => CredentialError::Busy,
                    mpsc::TrySendError::Disconnected(_) => CredentialError::Closed,
                })?;
        }
        response_receiver
            .recv_timeout(CALL_DEADLINE)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => CredentialError::TimedOut,
                mpsc::RecvTimeoutError::Disconnected => CredentialError::Closed,
            })?
    }

    pub fn shutdown(self) -> Result<(), CredentialError> {
        self.inner.stop_and_join()
    }
}

fn worker_main(receiver: mpsc::Receiver<Command>, mut backend: impl CredentialBackend) {
    let mut latest_generation = 0_u64;
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Set {
                generation,
                credential_id,
                material,
                response,
            } => {
                let result = check_generation(generation, &mut latest_generation).and_then(|()| {
                    let mut secret = material.into_secret()?;
                    let username = credential_username(&credential_id);
                    backend
                        .set(&username, &secret.0)
                        .map_err(map_backend_error)?;
                    Ok(std::mem::replace(&mut secret, SecretBytes::new(Vec::new())))
                });
                let _ = response.send(result);
            }
            Command::Get {
                generation,
                credential_id,
                response,
            } => {
                let result = check_generation(generation, &mut latest_generation).and_then(|()| {
                    backend
                        .get(&credential_username(&credential_id))
                        .map(SecretBytes::new)
                        .map_err(map_backend_error)
                });
                let _ = response.send(result);
            }
            Command::Verify {
                generation,
                credential_id,
                material,
                response,
            } => {
                let result = check_generation(generation, &mut latest_generation).and_then(|()| {
                    let expected = material.into_secret()?;
                    let actual = backend
                        .get(&credential_username(&credential_id))
                        .map(SecretBytes::new)
                        .map_err(map_backend_error)?;
                    if !actual.matches(&expected) {
                        return Err(CredentialError::Unavailable);
                    }
                    Ok(SecretBytes::new(Vec::new()))
                });
                let _ = response.send(result);
            }
            Command::Delete {
                generation,
                credential_id,
                response,
            } => {
                let result = check_generation(generation, &mut latest_generation).and_then(|()| {
                    backend
                        .delete(&credential_username(&credential_id))
                        .map_err(map_backend_error)?;
                    Ok(SecretBytes::new(Vec::new()))
                });
                let _ = response.send(result);
            }
            Command::Shutdown => break,
        }
    }
}

fn check_generation(generation: u64, latest: &mut u64) -> Result<(), CredentialError> {
    if generation < *latest {
        return Err(CredentialError::StaleGeneration);
    }
    *latest = generation;
    Ok(())
}

fn map_backend_error(error: BackendError) -> CredentialError {
    match error {
        BackendError::NotFound => CredentialError::NotFound,
        BackendError::Unavailable | BackendError::Failed => CredentialError::Unavailable,
    }
}

pub fn stable_credential_id(logical_id: &str) -> String {
    format!("credential-{:016x}", stable_hash(logical_id.as_bytes()))
}

pub fn stable_operation_id(run_id: &str, credential_id: &str) -> String {
    let mut bytes = Vec::with_capacity(run_id.len() + credential_id.len() + 1);
    bytes.extend_from_slice(run_id.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(credential_id.as_bytes());
    format!("operation-{:016x}", stable_hash(&bytes))
}

fn credential_username(credential_id: &str) -> String {
    format!("credential-{:016x}", stable_hash(credential_id.as_bytes()))
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn validate_credential_id(value: &str) -> Result<&str, CredentialError> {
    if value.len() == 27
        && value.starts_with("credential-")
        && value[11..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        Ok(value)
    } else {
        Err(CredentialError::InvalidInput)
    }
}

struct FixedSpec {
    setting: &'static str,
    first: &'static str,
    second: &'static str,
}

const FIXED_SPECS: &[FixedSpec] = &[
    FixedSpec {
        setting: "hubHostSecret",
        first: "hub",
        second: "hostSecret",
    },
    FixedSpec {
        setting: "secret",
        first: "hub",
        second: "clientSecret",
    },
    FixedSpec {
        setting: "alibabaCookie",
        first: "alibaba",
        second: "cookie",
    },
    FixedSpec {
        setting: "claudeWebCookie",
        first: "claude",
        second: "webCookie",
    },
    FixedSpec {
        setting: "clineApiKey",
        first: "cline",
        second: "apiKey",
    },
    FixedSpec {
        setting: "commandcodeCookie",
        first: "commandcode",
        second: "cookie",
    },
    FixedSpec {
        setting: "copilotApiToken",
        first: "copilot",
        second: "apiToken",
    },
    FixedSpec {
        setting: "deepseekApiKey",
        first: "deepseek",
        second: "apiKey",
    },
    FixedSpec {
        setting: "devinBearerToken",
        first: "devin",
        second: "bearerToken",
    },
    FixedSpec {
        setting: "factoryApiKey",
        first: "factory",
        second: "apiKey",
    },
    FixedSpec {
        setting: "kimiApiKey",
        first: "kimi",
        second: "apiKey",
    },
    FixedSpec {
        setting: "kimiWebAccessToken",
        first: "kimi",
        second: "webAccessToken",
    },
    FixedSpec {
        setting: "manusSessionToken",
        first: "manus",
        second: "sessionToken",
    },
    FixedSpec {
        setting: "minimaxApiKey",
        first: "minimax",
        second: "apiKey",
    },
    FixedSpec {
        setting: "minimaxApiProfiles",
        first: "minimax",
        second: "profiles",
    },
    FixedSpec {
        setting: "mistralCookie",
        first: "mistral",
        second: "cookie",
    },
    FixedSpec {
        setting: "museWebCookie",
        first: "muse",
        second: "webCookie",
    },
    FixedSpec {
        setting: "notionCookie",
        first: "notion",
        second: "cookie",
    },
    FixedSpec {
        setting: "ollamaApiKey",
        first: "ollama",
        second: "apiKey",
    },
    FixedSpec {
        setting: "ollamaCookie",
        first: "ollama",
        second: "cookie",
    },
    FixedSpec {
        setting: "opencodeCookie",
        first: "opencode",
        second: "cookie",
    },
    FixedSpec {
        setting: "opencodeProfiles",
        first: "opencode",
        second: "profiles",
    },
    FixedSpec {
        setting: "opendesignCookie",
        first: "opendesign",
        second: "cookie",
    },
    FixedSpec {
        setting: "openrouterProfiles",
        first: "openrouter",
        second: "profiles",
    },
    FixedSpec {
        setting: "qoderCookie",
        first: "qoder",
        second: "cookie",
    },
    FixedSpec {
        setting: "stepfunToken",
        first: "stepfun",
        second: "token",
    },
    FixedSpec {
        setting: "thirdPartyProfiles",
        first: "thirdparty",
        second: "profiles",
    },
    FixedSpec {
        setting: "traeAccessToken",
        first: "trae",
        second: "accessToken",
    },
    FixedSpec {
        setting: "traeCookie",
        first: "trae",
        second: "cookie",
    },
    FixedSpec {
        setting: "traeDeviceId",
        first: "trae",
        second: "deviceId",
    },
    FixedSpec {
        setting: "typesafeCookie",
        first: "typesafe",
        second: "cookie",
    },
    FixedSpec {
        setting: "volcengineAccessKeyId",
        first: "volcengine",
        second: "accessKeyId",
    },
    FixedSpec {
        setting: "volcengineAgentAccessKeyId",
        first: "volcengine",
        second: "agentAccessKeyId",
    },
    FixedSpec {
        setting: "volcengineAgentSecretAccessKey",
        first: "volcengine",
        second: "agentSecretAccessKey",
    },
    FixedSpec {
        setting: "volcengineSecretAccessKey",
        first: "volcengine",
        second: "secretAccessKey",
    },
    FixedSpec {
        setting: "zaiApiKey",
        first: "zai",
        second: "apiKey",
    },
    FixedSpec {
        setting: "zaiTeamApiKey",
        first: "zaiTeam",
        second: "apiKey",
    },
    FixedSpec {
        setting: "zaiTeamOrganizationId",
        first: "zaiTeam",
        second: "organizationId",
    },
    FixedSpec {
        setting: "zaiTeamProjectId",
        first: "zaiTeam",
        second: "projectId",
    },
    FixedSpec {
        setting: "zedCookie",
        first: "zed",
        second: "cookie",
    },
];

pub fn plan_import(
    credential_document: Option<&[u8]>,
    settings_document: Option<&[u8]>,
) -> Result<CredentialImportPlan, CredentialError> {
    if credential_document.is_some_and(|bytes| bytes.len() > MAX_DOCUMENT_BYTES)
        || settings_document.is_some_and(|bytes| bytes.len() > MAX_DOCUMENT_BYTES)
    {
        return Err(CredentialError::InvalidInput);
    }
    let settings = settings_document
        .map(parse_object)
        .transpose()?
        .unwrap_or_default();
    let stripped_settings = settings_document
        .map(|_| strip_settings_map(settings.clone()))
        .map(|value| serde_json::to_vec(&Value::Object(value)))
        .transpose()
        .map_err(|_| CredentialError::InvalidInput)?;

    let mut issues = Vec::new();
    let mut credentials = match credential_document {
        None => Map::new(),
        Some(bytes) => {
            let document = parse_object(bytes)?;
            if document.get("version") != Some(&Value::Number(1.into())) {
                issues.push(issue(
                    "document",
                    CredentialPlanIssueKind::UnsupportedDocument,
                ));
                return Ok(CredentialImportPlan {
                    transfers: Vec::new(),
                    issues,
                    stripped_settings,
                });
            }
            match document.get("credentials") {
                Some(Value::Object(credentials)) => credentials.clone(),
                Some(_) => {
                    issues.push(issue(
                        "credentials",
                        CredentialPlanIssueKind::InvalidSupportedBranch,
                    ));
                    Map::new()
                }
                None => Map::new(),
            }
        }
    };
    let authoritative = credentials.clone();
    let mut transfers = Vec::new();
    let mut seen_ids = BTreeSet::new();

    for spec in FIXED_SPECS {
        let path = if spec.first == "hub" {
            vec!["hub", spec.second]
        } else {
            vec!["providers", spec.first, spec.second]
        };
        let staged = value_at(&authoritative, &path).cloned();
        remove_at(&mut credentials, &path);
        let material = match staged {
            Some(value) if credential_value_present(&value) => Some(value),
            Some(_) => {
                issues.push(issue(
                    &path.join("/"),
                    CredentialPlanIssueKind::InvalidSupportedBranch,
                ));
                None
            }
            None => match settings.get(spec.setting).cloned() {
                Some(value) if credential_value_present(&value) => Some(value),
                Some(_) => {
                    issues.push(issue(
                        &format!("settings/{}", spec.setting),
                        CredentialPlanIssueKind::InvalidSupportedBranch,
                    ));
                    None
                }
                None => None,
            },
        };
        if let Some(material) = material {
            push_transfer(
                &mut transfers,
                &mut seen_ids,
                &path.join("/"),
                CredentialKind::Fixed,
                material,
            )?;
        }
    }

    for (provider, field, kind, object_only) in [
        ("claude", "cookie", CredentialKind::ClaudeAccount, false),
        ("mimo", "cookieHeader", CredentialKind::MimoAccount, false),
        (
            "antigravity",
            "credentials",
            CredentialKind::AntigravityAccount,
            true,
        ),
        (
            "workbuddy",
            "credentials",
            CredentialKind::WorkbuddyAccount,
            true,
        ),
    ] {
        collect_dynamic(
            &authoritative,
            &mut credentials,
            provider,
            field,
            kind,
            object_only,
            &mut transfers,
            &mut seen_ids,
            &mut issues,
        )?;
    }

    let mut unknown = Vec::new();
    if !credentials.is_empty() {
        collect_leaf_paths(&Value::Object(credentials), "credentials", &mut unknown);
    }
    issues.extend(
        unknown
            .into_iter()
            .map(|path| issue(&path, CredentialPlanIssueKind::UnknownBranch)),
    );
    Ok(CredentialImportPlan {
        transfers,
        issues,
        stripped_settings,
    })
}

fn parse_object(bytes: &[u8]) -> Result<Map<String, Value>, CredentialError> {
    match serde_json::from_slice(bytes).map_err(|_| CredentialError::InvalidInput)? {
        Value::Object(object) => Ok(object),
        _ => Err(CredentialError::InvalidInput),
    }
}

fn strip_settings_map(mut settings: Map<String, Value>) -> Map<String, Value> {
    for spec in FIXED_SPECS {
        settings.remove(spec.setting);
    }
    settings
}

fn value_at<'a>(root: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Value> {
    let mut value = root.get(path[0])?;
    for segment in &path[1..] {
        value = value.as_object()?.get(*segment)?;
    }
    Some(value)
}

fn remove_at(root: &mut Map<String, Value>, path: &[&str]) {
    fn remove(current: &mut Map<String, Value>, path: &[&str]) -> bool {
        if path.len() == 1 {
            current.remove(path[0]);
        } else if let Some(Value::Object(child)) = current.get_mut(path[0])
            && remove(child, &path[1..])
        {
            current.remove(path[0]);
        }
        current.is_empty()
    }
    remove(root, path);
}

#[allow(clippy::too_many_arguments)]
fn collect_dynamic(
    source: &Map<String, Value>,
    remaining: &mut Map<String, Value>,
    provider: &str,
    field: &str,
    kind: CredentialKind,
    object_only: bool,
    transfers: &mut Vec<CredentialTransfer>,
    seen_ids: &mut BTreeSet<String>,
    issues: &mut Vec<CredentialPlanIssue>,
) -> Result<(), CredentialError> {
    let Some(Value::Object(accounts)) = value_at(source, &["providers", provider, "accounts"])
    else {
        return Ok(());
    };
    for (account, account_value) in accounts {
        let safe_account = account.trim();
        if safe_account.is_empty()
            || matches!(safe_account, "__proto__" | "prototype" | "constructor")
        {
            issues.push(issue(
                &format!("providers/{provider}/accounts/{account}"),
                CredentialPlanIssueKind::InvalidSupportedBranch,
            ));
            continue;
        }
        let path = ["providers", provider, "accounts", account.as_str(), field];
        let material = account_value
            .as_object()
            .and_then(|object| object.get(field))
            .cloned();
        remove_at(remaining, &path);
        if let Some(material) = material.filter(credential_value_present) {
            if object_only && !material.is_object() {
                issues.push(issue(
                    &path.join("/"),
                    CredentialPlanIssueKind::InvalidSupportedBranch,
                ));
                continue;
            }
            push_transfer(transfers, seen_ids, &path.join("/"), kind, material)?;
        }
    }
    Ok(())
}

fn push_transfer(
    transfers: &mut Vec<CredentialTransfer>,
    seen_ids: &mut BTreeSet<String>,
    logical_id: &str,
    kind: CredentialKind,
    material: Value,
) -> Result<(), CredentialError> {
    if !credential_value_present(&material) {
        return Ok(());
    }
    let bytes = match material {
        Value::String(value) => value.into_bytes(),
        value => serde_json::to_vec(&value).map_err(|_| CredentialError::InvalidInput)?,
    };
    let material = CredentialMaterial::from_secret_bytes(bytes)?;
    let credential_id = stable_credential_id(logical_id);
    if !seen_ids.insert(credential_id.clone()) {
        return Err(CredentialError::InvalidInput);
    }
    transfers.push(CredentialTransfer {
        credential_id,
        kind,
        material,
    });
    Ok(())
}

fn credential_value_present(value: &Value) -> bool {
    match value {
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
        _ => false,
    }
}

fn collect_leaf_paths(value: &Value, path: &str, output: &mut Vec<String>) {
    match value {
        Value::Object(object) if !object.is_empty() => {
            for (key, value) in object {
                collect_leaf_paths(value, &format!("{path}/{key}"), output);
            }
        }
        _ => output.push(path.to_owned()),
    }
}

fn issue(path: &str, kind: CredentialPlanIssueKind) -> CredentialPlanIssue {
    CredentialPlanIssue {
        branch_id: format!("branch-{:016x}", stable_hash(path.as_bytes())),
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct FakeState {
        values: BTreeMap<String, Vec<u8>>,
        calls: Vec<&'static str>,
        unavailable: bool,
    }

    struct FakeBackend(Arc<Mutex<FakeState>>);

    impl CredentialBackend for FakeBackend {
        fn set(&mut self, username: &str, secret: &[u8]) -> Result<(), BackendError> {
            let mut state = self.0.lock().map_err(|_| BackendError::Failed)?;
            state.calls.push("set");
            if state.unavailable {
                return Err(BackendError::Unavailable);
            }
            state.values.insert(username.to_owned(), secret.to_vec());
            Ok(())
        }

        fn get(&mut self, username: &str) -> Result<Vec<u8>, BackendError> {
            let mut state = self.0.lock().map_err(|_| BackendError::Failed)?;
            state.calls.push("get");
            if state.unavailable {
                return Err(BackendError::Unavailable);
            }
            state
                .values
                .get(username)
                .cloned()
                .ok_or(BackendError::NotFound)
        }

        fn delete(&mut self, username: &str) -> Result<(), BackendError> {
            let mut state = self.0.lock().map_err(|_| BackendError::Failed)?;
            state.calls.push("delete");
            if state.unavailable {
                return Err(BackendError::Unavailable);
            }
            state
                .values
                .remove(username)
                .ok_or(BackendError::NotFound)?;
            Ok(())
        }
    }

    #[test]
    fn fake_backend_is_serialized_redacted_and_generation_fenced() {
        let state = Arc::new(Mutex::new(FakeState::default()));
        let service = CredentialService::start_with_backend(FakeBackend(state.clone())).unwrap();
        let id = stable_credential_id("providers/claude/webCookie");
        assert!(!id.contains("claude"));
        let expected = service
            .set(
                2,
                &id,
                CredentialMaterial::from_secret_bytes(b"sentinel".to_vec()).unwrap(),
            )
            .unwrap();
        let actual = service.get(2, &id).unwrap();
        assert!(actual.matches(&expected));
        assert_eq!(service.get(1, &id), Err(CredentialError::StaleGeneration));
        service.delete(2, &id).unwrap();
        assert_eq!(format!("{expected:?}"), "SecretBytes([redacted])");
        assert_eq!(state.lock().unwrap().calls, ["set", "get", "delete"]);
        service.shutdown().unwrap();
    }

    #[test]
    fn backend_failures_are_redacted_as_unavailable() {
        let state = Arc::new(Mutex::new(FakeState {
            unavailable: true,
            ..FakeState::default()
        }));
        let service = CredentialService::start_with_backend(FakeBackend(state)).unwrap();
        let id = stable_credential_id("hub/clientSecret");
        let error = service
            .set(
                0,
                &id,
                CredentialMaterial::from_secret_bytes(b"sentinel".to_vec()).unwrap(),
            )
            .unwrap_err();
        assert_eq!(error, CredentialError::Unavailable);
        assert!(!format!("{error:?}").contains("sentinel"));
        service.shutdown().unwrap();
    }

    #[test]
    fn import_plan_prefers_staged_values_and_reports_unknowns_without_paths() {
        let credentials = br#"{
            "version":1,
            "credentials":{
                "hub":{"hostSecret":"staged"},
                "providers":{
                    "claude":{"accounts":{"account-a":{"cookie":"cookie","extra":"kept"}}},
                    "antigravity":{"accounts":{"google":{"credentials":{"refreshToken":"secret"}}}},
                    "future":{"token":"preserved"}
                }
            },
            "unknownTopLevel":{"kept":true}
        }"#;
        let settings = br#"{
            "hubHostSecret":"legacy-loses",
            "secret":"fallback",
            "unknown":{"kept":true}
        }"#;
        let plan = plan_import(Some(credentials), Some(settings)).unwrap();
        assert_eq!(plan.transfers.len(), 4);
        assert_eq!(plan.issues.len(), 2);
        assert!(
            plan.issues
                .iter()
                .all(|issue| !issue.branch_id.contains("future")
                    && !issue.branch_id.contains("extra"))
        );
        let stripped: Value =
            serde_json::from_slice(plan.stripped_settings.as_ref().unwrap()).unwrap();
        assert!(stripped.get("hubHostSecret").is_none());
        assert!(stripped.get("secret").is_none());
        assert_eq!(stripped["unknown"]["kept"], true);
    }
}
