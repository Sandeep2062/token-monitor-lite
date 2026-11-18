//! Ordered display data adapted from Token Monitor's pinned MIT catalogs.
//! Catalog membership is not a claim of native reader/provider support.

use serde::Deserialize;
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub clients: Vec<Client>,
    pub providers: Vec<Provider>,
    pub provider_by_client: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Client {
    pub id: String,
    pub label: String,
    #[serde(default = "default_tracked")]
    pub default_tracked: bool,
    #[serde(default)]
    pub fork_only: bool,
}

fn default_tracked() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub settings_label: Option<String>,
}

impl Provider {
    pub fn settings_label(&self) -> &str {
        self.settings_label.as_deref().unwrap_or(&self.label)
    }
}

impl Catalog {
    pub fn provider_for_client(&self, client: &str) -> Option<&Provider> {
        let client = client.trim().to_ascii_lowercase();
        let provider = self
            .provider_by_client
            .get(&client)
            .map_or(client.as_str(), String::as_str);
        self.providers.iter().find(|row| row.id == provider)
    }
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("invalid bundled catalog: {0}")]
    Decode(#[from] serde_json::Error),
}

pub fn load() -> Result<Catalog, CatalogError> {
    Ok(serde_json::from_str(include_str!(
        "../assets/catalog.json"
    ))?)
}

/// Whether the catalog tracks this client.
///
/// The catalog is the single source of truth for which clients exist. Every
/// hand-maintained list that used to duplicate this drifted: the settings
/// validator accepted 25 ids while the catalog held 53, so saving a selection
/// that included any of the other 28 was rejected as `EnabledClients:
/// Unsupported` and the application came back up with nothing monitored.
pub fn tracks_client(client: &str) -> bool {
    !client.is_empty() && catalog().clients.iter().any(|row| row.id == client)
}

/// The bundled catalog, decoded once.
pub fn catalog() -> &'static Catalog {
    static CATALOG: std::sync::OnceLock<Catalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| load().expect("the bundled catalog must decode"))
}
