//! Reading addressed content through a responder.
//!
//! Whoever holds a bucket's credentials answers requests for the addresses in
//! it; everyone else reads through that responder over NATS and never learns
//! which bucket the bytes came from. This is the reader's half of the exchange
//! and the wire rules both halves share. Replies are paged, not streamed: a
//! NATS payload is capped, so a request names an offset, every reply states
//! the whole size, the last one says it is the last, and a refusal travels as
//! a header — never as an empty body a reader could take for content.

use crate::content_address::ContentAddress;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Bytes returned in one reply: under the default 1 MiB payload cap, with
/// room for headers and framing.
pub const WINDOW_BYTES: usize = 512 * 1024;

/// Header stating the whole content's size, on every reply. A reader bounds
/// what it accumulates by it.
pub const HEADER_TOTAL: &str = "Nsed-Content-Total";
/// Header set to `true` on the reply that completes the content.
pub const HEADER_EOF: &str = "Nsed-Content-Eof";
/// Header carrying a refusal.
pub const HEADER_ERROR: &str = "Nsed-Content-Error";
/// Header presenting a capability on a request.
pub const HEADER_CAPABILITY: &str = "Nsed-Content-Capability";

/// A request for one window of an addressed object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContentRequest {
    /// The address to read.
    pub address: String,
    /// Offset into the content, in bytes.
    #[serde(default)]
    pub offset: usize,
}

/// Headers presenting `capability` on a request.
pub fn capability_headers(capability: &str) -> async_nats::HeaderMap {
    let mut headers = async_nats::HeaderMap::new();
    headers.insert(HEADER_CAPABILITY, capability);
    headers
}

/// Fetch the whole of an addressed object through the responder on `subject`.
///
/// Loops over windows until the responder reports the end, then checks the
/// bytes hash to the address: the reply crossed an open subject from a party
/// this side does not authenticate, and matching a length is trivial.
pub async fn fetch(
    client: &async_nats::Client,
    subject: &str,
    address: &ContentAddress,
    capability: Option<&str>,
) -> Result<Vec<u8>> {
    let mut content: Vec<u8> = Vec::new();

    loop {
        let request = ContentRequest {
            address: address.to_string(),
            offset: content.len(),
        };
        let reply = client
            .request_with_headers(
                subject.to_string(),
                capability.map(capability_headers).unwrap_or_default(),
                serde_json::to_vec(&request)?.into(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("request content")?;

        let headers = reply.headers.unwrap_or_default();
        if let Some(error) = headers.get(HEADER_ERROR) {
            anyhow::bail!("responder refused {address}: {error}");
        }
        if let Some(total) = headers
            .get(HEADER_TOTAL)
            .and_then(|v| v.as_str().parse::<usize>().ok())
            && content.len() + reply.payload.len() > total
        {
            anyhow::bail!("responder served more than the {total} bytes it says {address} holds");
        }

        let eof = headers.get(HEADER_EOF).map(|v| v.as_str()) == Some("true");
        if reply.payload.is_empty() && !eof {
            anyhow::bail!("responder returned no bytes and no end for {address}");
        }
        content.extend_from_slice(&reply.payload);

        if eof {
            let actual = crate::nats_utils::sha256_hex_bytes(&content);
            if actual != address.digest() {
                anyhow::bail!(
                    "content served for {address} hashes to {actual}, so it is not what was named"
                );
            }
            return Ok(content);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_address::ContentAddress;
    use futures::StreamExt as _;

    async fn client() -> Option<async_nats::Client> {
        let url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".into());
        match async_nats::connect(&url).await {
            Ok(client) => Some(client),
            Err(e) => {
                assert!(
                    std::env::var_os("REQUIRE_NATS").is_none(),
                    "REQUIRE_NATS is set and {url} is unreachable: {e}"
                );
                eprintln!("skipping: no broker at {url}");
                None
            }
        }
    }

    fn address_of(content: &[u8]) -> ContentAddress {
        ContentAddress::new(
            "nats-obj",
            "test",
            crate::nats_utils::sha256_hex_bytes(content),
        )
        .expect("address")
    }

    /// One reply the fake responder sends for a request.
    struct Reply {
        headers: Vec<(&'static str, String)>,
        payload: Vec<u8>,
    }

    /// A responder that answers each request with whatever `script` makes of
    /// it, and reports the capability header it was shown.
    async fn responder(
        client: &async_nats::Client,
        script: impl Fn(&ContentRequest) -> Reply + Send + 'static,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<Option<String>>) {
        let subject = format!("test.content.{}", uuid::Uuid::new_v4().simple());
        let mut requests = client.subscribe(subject.clone()).await.expect("subscribe");
        let (seen, capabilities) = tokio::sync::mpsc::unbounded_channel();
        let replier = client.clone();
        tokio::spawn(async move {
            while let Some(message) = requests.next().await {
                let request: ContentRequest =
                    serde_json::from_slice(&message.payload).expect("a content request");
                let _ = seen.send(
                    message
                        .headers
                        .as_ref()
                        .and_then(|h| h.get(HEADER_CAPABILITY))
                        .map(|v| v.as_str().to_string()),
                );
                let reply = script(&request);
                let mut headers = async_nats::HeaderMap::new();
                for (name, value) in reply.headers {
                    headers.insert(name, value.as_str());
                }
                if let Some(to) = message.reply {
                    let _ = replier
                        .publish_with_headers(to, headers, reply.payload.into())
                        .await;
                }
            }
        });
        client.flush().await.expect("flush");
        (subject, capabilities)
    }

    /// Serves `content` in windows of `window` bytes, the way a responder does.
    fn windows_of(content: Vec<u8>, window: usize) -> impl Fn(&ContentRequest) -> Reply {
        move |request| {
            let end = (request.offset + window).min(content.len());
            Reply {
                headers: vec![
                    (HEADER_TOTAL, content.len().to_string()),
                    (HEADER_EOF, (end == content.len()).to_string()),
                ],
                payload: content[request.offset..end].to_vec(),
            }
        }
    }

    #[tokio::test]
    async fn windows_are_reassembled_and_the_capability_is_presented() {
        let Some(client) = client().await else { return };
        let content = b"the whole of an addressed object, in pieces".to_vec();
        let (subject, mut shown) = responder(&client, windows_of(content.clone(), 8)).await;

        let fetched = fetch(&client, &subject, &address_of(&content), Some("cap-1"))
            .await
            .expect("fetched");
        assert_eq!(fetched, content);
        assert_eq!(shown.recv().await.flatten().as_deref(), Some("cap-1"));
    }

    #[tokio::test]
    async fn a_refusal_is_an_error_never_empty_content() {
        let Some(client) = client().await else { return };
        let (subject, _) = responder(&client, |_| Reply {
            headers: vec![(HEADER_ERROR, "not yours".into())],
            payload: Vec::new(),
        })
        .await;
        let err = fetch(&client, &subject, &address_of(b"x"), None)
            .await
            .expect_err("refused");
        assert!(err.to_string().contains("not yours"), "{err}");
    }

    /// A responder can lie; the reader bounds its buffer, refuses to loop on
    /// nothing, and proves the bytes are the ones the address names.
    #[tokio::test]
    async fn a_responder_cannot_overfill_stall_or_substitute() {
        let Some(client) = client().await else { return };
        let named = b"what was named".to_vec();

        let (subject, _) = responder(&client, |_| Reply {
            headers: vec![(HEADER_TOTAL, "4".into())],
            payload: b"more than four".to_vec(),
        })
        .await;
        let err = fetch(&client, &subject, &address_of(&named), None)
            .await
            .expect_err("overfilled");
        assert!(err.to_string().contains("more than the 4 bytes"), "{err}");

        let (subject, _) = responder(&client, |_| Reply {
            headers: vec![(HEADER_TOTAL, "14".into())],
            payload: Vec::new(),
        })
        .await;
        let err = fetch(&client, &subject, &address_of(&named), None)
            .await
            .expect_err("stalled");
        assert!(err.to_string().contains("no bytes and no end"), "{err}");

        let (subject, _) = responder(&client, windows_of(b"something else".to_vec(), 64)).await;
        let err = fetch(&client, &subject, &address_of(&named), None)
            .await
            .expect_err("substituted");
        assert!(err.to_string().contains("not what was named"), "{err}");
    }
}
