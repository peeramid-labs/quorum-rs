//! Where a feed's channels, records and indexes are kept.
//!
//! [`FeedStore`] is the storage seam under the crate root: the rules (drafts
//! are not served, a slug belongs to whoever took it first, a message id names
//! the first record that carried it) live at the crate root and hold for every store;
//! a store only keeps rows and answers the primitive questions below.
//!
//! The deployed store, on JetStream, lives in `nsed-content`.
//! [`MemoryFeedStore`] is the reference the conformance suite
//! (`crate::conformance`) is written against.

use anyhow::Result;

use crate::{Channel, Record, SlugEntry};

/// Storage for channels, their records, and the slug and message-id indexes.
///
/// Record ids are assigned by the store, per channel, ascending from 1.
#[async_trait::async_trait]
pub trait FeedStore: Send + Sync {
    /// Register `channel` unless its address is registered already, and make
    /// its record storage ready. A second registration changes nothing.
    async fn register_channel(&self, channel: &Channel) -> Result<()>;

    async fn channel(&self, address: &str) -> Result<Option<Channel>>;

    /// Every registered channel, in no particular order.
    async fn channels(&self) -> Result<Vec<Channel>>;

    /// Append `record` to `address`, returning the id it was given.
    async fn append(&self, address: &str, record: &Record) -> Result<u64>;

    /// The record stored under `id`, drafts included, with its id set.
    async fn record(&self, address: &str, id: u64) -> Result<Option<Record>>;

    /// Up to `limit` records that are not drafts, with ids below `before`
    /// (all ids when `None`), newest first.
    async fn newest_before(
        &self,
        address: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Record>>;

    /// Up to `limit` records that are not drafts, from id `first` on, oldest
    /// first.
    async fn oldest_from(&self, address: &str, first: u64, limit: usize) -> Result<Vec<Record>>;

    /// The slug index entry and its revision, for a compare-and-set.
    async fn slug(&self, address: &str, slug: &str) -> Result<Option<(SlugEntry, u64)>>;

    /// Create the slug entry; `false` when one exists.
    async fn create_slug(&self, address: &str, slug: &str, entry: &SlugEntry) -> Result<bool>;

    /// Replace the slug entry if it is still at `revision`; `false` otherwise.
    async fn update_slug(
        &self,
        address: &str,
        slug: &str,
        entry: &SlugEntry,
        revision: u64,
    ) -> Result<bool>;

    /// The record id a message id points at: the channel's first claim when
    /// `sender` is `None`, that sender's first claim otherwise.
    async fn message_id(
        &self,
        address: &str,
        sender: Option<&str>,
        message_id: &str,
    ) -> Result<Option<u64>>;

    /// Point a message id at `id` unless it points somewhere already.
    async fn create_message_id(
        &self,
        address: &str,
        sender: Option<&str>,
        message_id: &str,
        id: u64,
    ) -> Result<()>;
}

// ── Memory ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Memory {
    channels: std::collections::BTreeMap<String, Channel>,
    /// Per channel, records in id order; index `i` holds id `i + 1`.
    records: std::collections::HashMap<String, Vec<Record>>,
    slugs: std::collections::HashMap<(String, String), (SlugEntry, u64)>,
    message_ids: std::collections::HashMap<(String, Option<String>, String), u64>,
}

/// Everything in a mutex: the reference behaviour, and the store unit tests
/// run on without a broker.
#[derive(Default, Clone)]
pub struct MemoryFeedStore(std::sync::Arc<std::sync::Mutex<Memory>>);

impl MemoryFeedStore {
    fn with<T>(&self, f: impl FnOnce(&mut Memory) -> T) -> T {
        // Every operation leaves the maps consistent before it can panic, so a
        // poisoned lock still guards valid data.
        f(&mut self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner))
    }
}

#[async_trait::async_trait]
impl FeedStore for MemoryFeedStore {
    async fn register_channel(&self, channel: &Channel) -> Result<()> {
        self.with(|m| {
            m.channels
                .entry(channel.address.clone())
                .or_insert_with(|| channel.clone());
            m.records.entry(channel.address.clone()).or_default();
        });
        Ok(())
    }

    async fn channel(&self, address: &str) -> Result<Option<Channel>> {
        Ok(self.with(|m| m.channels.get(address).cloned()))
    }

    async fn channels(&self) -> Result<Vec<Channel>> {
        Ok(self.with(|m| m.channels.values().cloned().collect()))
    }

    async fn append(&self, address: &str, record: &Record) -> Result<u64> {
        Ok(self.with(|m| {
            let records = m.records.entry(address.to_string()).or_default();
            records.push(record.clone());
            records.len() as u64
        }))
    }

    async fn record(&self, address: &str, id: u64) -> Result<Option<Record>> {
        Ok(self.with(|m| {
            let index = usize::try_from(id).ok()?.checked_sub(1)?;
            let mut record = m.records.get(address)?.get(index)?.clone();
            record.id = id;
            Some(record)
        }))
    }

    async fn newest_before(
        &self,
        address: &str,
        before: Option<u64>,
        limit: usize,
    ) -> Result<Vec<Record>> {
        Ok(self.with(|m| {
            let Some(records) = m.records.get(address) else {
                return Vec::new();
            };
            let end = before.map_or(records.len(), |b| {
                (b.saturating_sub(1) as usize).min(records.len())
            });
            records[..end]
                .iter()
                .enumerate()
                .rev()
                .filter(|(_, r)| !r.is_draft())
                .take(limit)
                .map(|(i, r)| Record {
                    id: i as u64 + 1,
                    ..r.clone()
                })
                .collect()
        }))
    }

    async fn oldest_from(&self, address: &str, first: u64, limit: usize) -> Result<Vec<Record>> {
        Ok(self.with(|m| {
            let Some(records) = m.records.get(address) else {
                return Vec::new();
            };
            let start = usize::try_from(first.saturating_sub(1)).unwrap_or(usize::MAX);
            records
                .iter()
                .enumerate()
                .skip(start)
                .filter(|(_, r)| !r.is_draft())
                .take(limit)
                .map(|(i, r)| Record {
                    id: i as u64 + 1,
                    ..r.clone()
                })
                .collect()
        }))
    }

    async fn slug(&self, address: &str, slug: &str) -> Result<Option<(SlugEntry, u64)>> {
        Ok(self.with(|m| {
            m.slugs
                .get(&(address.to_string(), slug.to_string()))
                .cloned()
        }))
    }

    async fn create_slug(&self, address: &str, slug: &str, entry: &SlugEntry) -> Result<bool> {
        Ok(self.with(|m| {
            let key = (address.to_string(), slug.to_string());
            if m.slugs.contains_key(&key) {
                return false;
            }
            m.slugs.insert(key, (entry.clone(), 1));
            true
        }))
    }

    async fn update_slug(
        &self,
        address: &str,
        slug: &str,
        entry: &SlugEntry,
        revision: u64,
    ) -> Result<bool> {
        Ok(self.with(
            |m| match m.slugs.get_mut(&(address.to_string(), slug.to_string())) {
                Some(held) if held.1 == revision => {
                    *held = (entry.clone(), revision + 1);
                    true
                }
                _ => false,
            },
        ))
    }

    async fn message_id(
        &self,
        address: &str,
        sender: Option<&str>,
        message_id: &str,
    ) -> Result<Option<u64>> {
        Ok(self.with(|m| {
            m.message_ids
                .get(&(
                    address.to_string(),
                    sender.map(str::to_string),
                    message_id.to_string(),
                ))
                .copied()
        }))
    }

    async fn create_message_id(
        &self,
        address: &str,
        sender: Option<&str>,
        message_id: &str,
        id: u64,
    ) -> Result<()> {
        self.with(|m| {
            m.message_ids
                .entry((
                    address.to_string(),
                    sender.map(str::to_string),
                    message_id.to_string(),
                ))
                .or_insert(id);
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(subject: &str, draft: bool) -> Record {
        let mut r = crate::conformance::a_record("a@noolog.io", "c@noolog.io", subject);
        if draft {
            r.keywords = vec![crate::DRAFT.to_string()];
        }
        r
    }

    #[tokio::test]
    async fn a_record_is_found_only_by_an_id_the_channel_assigned() -> Result<()> {
        let store = MemoryFeedStore::default();
        assert_eq!(store.append("c@noolog.io", &record("one", false)).await?, 1);
        assert_eq!(store.append("c@noolog.io", &record("two", false)).await?, 2);
        let two = store.record("c@noolog.io", 2).await?.expect("stored");
        assert_eq!((two.id, two.subject.as_str()), (2, "two"));
        assert!(
            store.record("c@noolog.io", 0).await?.is_none(),
            "ids start at 1"
        );
        assert!(
            store.record("c@noolog.io", 3).await?.is_none(),
            "past the end"
        );
        assert!(store.record("other@noolog.io", 1).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn paging_bounds_hold_at_both_ends_and_skip_drafts() -> Result<()> {
        let store = MemoryFeedStore::default();
        for (subject, draft) in [("one", false), ("draft", true), ("three", false)] {
            store.append("c@noolog.io", &record(subject, draft)).await?;
        }
        let ids = |rs: Vec<Record>| rs.into_iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(
            ids(store.newest_before("c@noolog.io", None, 10).await?),
            [3, 1]
        );
        assert_eq!(
            ids(store.newest_before("c@noolog.io", Some(99), 10).await?),
            [3, 1]
        );
        assert!(
            store
                .newest_before("c@noolog.io", Some(1), 10)
                .await?
                .is_empty()
        );
        assert!(
            store
                .newest_before("c@noolog.io", Some(0), 10)
                .await?
                .is_empty()
        );
        assert_eq!(ids(store.oldest_from("c@noolog.io", 0, 10).await?), [1, 3]);
        assert!(store.oldest_from("c@noolog.io", 4, 10).await?.is_empty());
        assert!(
            store
                .oldest_from("nobody@noolog.io", 1, 10)
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_slug_is_created_once_and_replaced_only_at_its_revision() -> Result<()> {
        let store = MemoryFeedStore::default();
        let first = SlugEntry {
            id: 1,
            from: "a@noolog.io".into(),
        };
        let second = SlugEntry {
            id: 2,
            from: "a@noolog.io".into(),
        };
        assert!(
            !store.update_slug("c@noolog.io", "s", &first, 1).await?,
            "nothing to update"
        );
        assert!(store.create_slug("c@noolog.io", "s", &first).await?);
        assert!(
            !store.create_slug("c@noolog.io", "s", &second).await?,
            "already held"
        );
        let (_, revision) = store.slug("c@noolog.io", "s").await?.expect("held");
        assert!(
            !store
                .update_slug("c@noolog.io", "s", &second, revision + 1)
                .await?,
            "stale"
        );
        assert!(
            store
                .update_slug("c@noolog.io", "s", &second, revision)
                .await?
        );
        let (now, next) = store.slug("c@noolog.io", "s").await?.expect("held");
        assert_eq!((now, next), (second, revision + 1));
        Ok(())
    }

    #[tokio::test]
    async fn a_message_id_keeps_its_first_claim_per_sender() -> Result<()> {
        let store = MemoryFeedStore::default();
        store
            .create_message_id("c@noolog.io", None, "<m>", 1)
            .await?;
        store
            .create_message_id("c@noolog.io", None, "<m>", 2)
            .await?;
        store
            .create_message_id("c@noolog.io", Some("b@noolog.io"), "<m>", 3)
            .await?;
        assert_eq!(store.message_id("c@noolog.io", None, "<m>").await?, Some(1));
        assert_eq!(
            store
                .message_id("c@noolog.io", Some("b@noolog.io"), "<m>")
                .await?,
            Some(3)
        );
        assert_eq!(
            store
                .message_id("c@noolog.io", Some("x@noolog.io"), "<m>")
                .await?,
            None
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_channel_keeps_the_date_it_was_first_registered() -> Result<()> {
        let store = MemoryFeedStore::default();
        let first = Channel {
            address: "c@noolog.io".into(),
            created_at: "t1".into(),
        };
        let later = Channel {
            address: "c@noolog.io".into(),
            created_at: "t2".into(),
        };
        store.register_channel(&first).await?;
        store.register_channel(&later).await?;
        assert_eq!(store.channel("c@noolog.io").await?, Some(first));
        assert_eq!(store.channels().await?.len(), 1);
        assert!(store.channel("other@noolog.io").await?.is_none());
        Ok(())
    }
}
