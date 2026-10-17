//! JMAP wire types: the core protocol (RFC 8620) and the mail data types
//! (RFC 8621) this project serves, written from the RFC texts.
//!
//! Types only: no transport and no server logic. Properties a type does not
//! name (vendor extensions such as `noolog:blog`, header forms such as
//! `header:List-Post:asURLs`) are kept in an `extra` map, so nothing a peer
//! sends is lost on a round trip.

mod core;
mod id;
mod mail;
mod methods;
mod session;

pub use crate::core::{
    Invocation, MethodError, ProblemDetails, Request, Response, ResultReference,
};
pub use id::{Id, InvalidId};
pub use mail::{
    Email, EmailAddress, EmailBodyPart, EmailBodyValue, EmailFilterCondition, Mailbox,
    MailboxRights, Thread,
};
pub use methods::{
    Comparator, Filter, GetRequest, GetResponse, QueryRequest, QueryResponse, SetError, SetRequest,
    SetResponse,
};
pub use session::{Account, CoreCapability, Session};

/// The core capability (RFC 8620 §2).
pub const CORE: &str = "urn:ietf:params:jmap:core";
/// The mail capability (RFC 8621 §1.3.1).
pub const MAIL: &str = "urn:ietf:params:jmap:mail";

/// Properties a type does not name, by name.
pub type Extra = serde_json::Map<String, serde_json::Value>;
