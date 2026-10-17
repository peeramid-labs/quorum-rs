//! Requests, responses and errors (RFC 8620 §3).

use std::collections::BTreeMap;

use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use crate::{Extra, Id};

/// A method call or response: `[name, arguments, method call id]` (§3.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Invocation {
    pub name: String,
    pub arguments: serde_json::Map<String, serde_json::Value>,
    pub call_id: String,
}

impl Invocation {
    /// An `error` response to call `call_id` (§3.6.2).
    pub fn error(call_id: &str, error: MethodError) -> Self {
        let arguments = match serde_json::to_value(error) {
            Ok(serde_json::Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        Self {
            name: "error".into(),
            arguments,
            call_id: call_id.into(),
        }
    }
}

impl Serialize for Invocation {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (&self.name, &self.arguments, &self.call_id).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Invocation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (name, arguments, call_id) =
            <(String, serde_json::Value, String)>::deserialize(deserializer)?;
        let serde_json::Value::Object(arguments) = arguments else {
            return Err(D::Error::custom("invocation arguments must be an object"));
        };
        Ok(Self {
            name,
            arguments,
            call_id,
        })
    }
}

/// A request to the API endpoint (§3.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub using: Vec<String>,
    pub method_calls: Vec<Invocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_ids: Option<BTreeMap<String, Id>>,
}

/// The API endpoint's answer (§3.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub method_responses: Vec<Invocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_ids: Option<BTreeMap<String, Id>>,
    pub session_state: String,
}

/// The arguments of an `error` response (§3.6.2): `serverFail`,
/// `unknownMethod`, `invalidArguments`, `accountNotFound`, … or a type a
/// later specification defines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MethodError {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl MethodError {
    /// An error of type `kind`, with no description.
    pub fn new(kind: &str) -> Self {
        Self {
            kind: kind.into(),
            description: None,
            extra: Extra::new(),
        }
    }

    /// The same error, described for a developer (not localised).
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// A request-level error body, as RFC 7807 problem details (§3.6.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProblemDetails {
    #[serde(rename = "type")]
    pub kind: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The limit exceeded, for `urn:ietf:params:jmap:error:limit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
}

/// An argument taken from an earlier response in the same request (§3.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultReference {
    pub result_of: String,
    pub name: String,
    pub path: String,
}
