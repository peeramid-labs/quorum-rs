//! The Session resource (RFC 8620 §2).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{CORE, Extra, Id};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    /// Capability URI to its server-wide properties.
    pub capabilities: Extra,
    pub accounts: BTreeMap<Id, Account>,
    pub primary_accounts: BTreeMap<String, Id>,
    pub username: String,
    pub api_url: String,
    pub download_url: String,
    pub upload_url: String,
    pub event_source_url: String,
    pub state: String,
}

impl Session {
    /// The `urn:ietf:params:jmap:core` capability, if present and well formed.
    pub fn core(&self) -> Option<CoreCapability> {
        serde_json::from_value(self.capabilities.get(CORE)?.clone()).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub name: String,
    pub is_personal: bool,
    pub is_read_only: bool,
    pub account_capabilities: Extra,
}

/// The server's limits under `urn:ietf:params:jmap:core`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoreCapability {
    pub max_size_upload: u64,
    pub max_concurrent_upload: u64,
    pub max_size_request: u64,
    pub max_concurrent_requests: u64,
    pub max_calls_in_request: u64,
    pub max_objects_in_get: u64,
    pub max_objects_in_set: u64,
    pub collation_algorithms: Vec<String>,
}
