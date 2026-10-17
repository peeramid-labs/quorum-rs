//! The standard method arguments (RFC 8620 §5): `/get`, `/set`, `/query`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Extra, Id};

/// `Foo/get` arguments (§5.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetRequest {
    pub account_id: Id,
    /// `None`: every record.
    #[serde(default)]
    pub ids: Option<Vec<Id>>,
    /// `None`: every property.
    #[serde(default)]
    pub properties: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// `Foo/get` response (§5.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetResponse<T> {
    pub account_id: Id,
    pub state: String,
    pub list: Vec<T>,
    pub not_found: Vec<Id>,
}

/// `Foo/set` arguments (§5.3). An update is a PatchObject: JSON Pointer
/// paths to values.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetRequest<T> {
    pub account_id: Id,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub if_in_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<BTreeMap<String, T>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<BTreeMap<Id, Extra>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroy: Option<Vec<Id>>,
}

/// `Foo/set` response (§5.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetResponse<T> {
    pub account_id: Id,
    #[serde(default)]
    pub old_state: Option<String>,
    pub new_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<BTreeMap<String, T>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<BTreeMap<Id, Option<T>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destroyed: Option<Vec<Id>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_created: Option<BTreeMap<String, SetError>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_updated: Option<BTreeMap<Id, SetError>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_destroyed: Option<BTreeMap<Id, SetError>>,
}

/// Why one create, update or destroy failed (§5.3): `forbidden`,
/// `overQuota`, `tooLarge`, `rateLimit`, `notFound`, `invalidPatch`,
/// `willDestroy`, `invalidProperties`, `singleton`, or a type a later
/// specification defines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetError {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The invalid properties, for `invalidProperties`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<Vec<String>>,
}

impl SetError {
    /// An error of type `kind`, with no description.
    pub fn new(kind: &str) -> Self {
        Self {
            kind: kind.into(),
            description: None,
            properties: None,
        }
    }

    /// The same error, described for a developer (not localised).
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The same error, naming the offending properties.
    pub fn with_properties<S: Into<String>>(
        mut self,
        properties: impl IntoIterator<Item = S>,
    ) -> Self {
        self.properties = Some(properties.into_iter().map(Into::into).collect());
        self
    }
}

/// `Foo/query` arguments (§5.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryRequest<F> {
    pub account_id: Id,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub filter: Option<Filter<F>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<Vec<Comparator>>,
    #[serde(default)]
    pub position: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Id>,
    #[serde(default)]
    pub anchor_offset: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    #[serde(default)]
    pub calculate_total: bool,
}

fn none<T>() -> Option<T> {
    None
}

/// A filter: a condition, or an operator (`AND`, `OR`, `NOT`) over filters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Filter<F> {
    Operator {
        operator: String,
        conditions: Vec<Filter<F>>,
    },
    Condition(F),
}

/// One sort key (§5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Comparator {
    pub property: String,
    #[serde(default = "ascending")]
    pub is_ascending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collation: Option<String>,
}

fn ascending() -> bool {
    true
}

/// `Foo/query` response (§5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResponse {
    pub account_id: Id,
    pub query_state: String,
    pub can_calculate_changes: bool,
    pub position: u64,
    pub ids: Vec<Id>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}
