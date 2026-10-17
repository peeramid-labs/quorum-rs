//! JMAP core (RFC 8620): the data types of the protocol, written from the
//! RFC text.
//!
//! Types only for now: no transport and no request processing. Properties a
//! type does not name (vendor extensions such as `vendor:property`) are kept
//! in an `extra` map, so nothing a peer sends is lost on a round trip. Data
//! types of other capabilities build on these: mail is `jmap-mail`.

mod core;
mod id;
mod methods;
mod session;

pub use crate::core::{
    Invocation, MethodError, ProblemDetails, Request, Response, ResultReference,
};
pub use id::{Id, InvalidId};
pub use methods::{
    Comparator, Filter, GetRequest, GetResponse, QueryRequest, QueryResponse, SetError, SetRequest,
    SetResponse,
};
pub use session::{Account, CoreCapability, Session};

/// The core capability (RFC 8620 §2).
pub const CORE: &str = "urn:ietf:params:jmap:core";

/// Properties a type does not name, by name.
pub type Extra = serde_json::Map<String, serde_json::Value>;
