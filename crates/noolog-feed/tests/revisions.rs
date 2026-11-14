//! Editing a post through the public API: the revision chain, the refusals and
//! what readers see, on the reference store.

use noolog_feed::blog::Blog;
use noolog_feed::conformance::{a_channel, a_record};
use noolog_feed::{
    MemoryFeedStore, NotLatest, ensure_channel, get_by_slug, get_record, page, post, revise,
    revision, superseded_by,
};

const CHANNEL: &str = "revisions@noolog.io";

async fn entry(store: &MemoryFeedStore) -> noolog_feed::Record {
    ensure_channel(store, &a_channel(CHANNEL)).await.unwrap();
    let mut first = a_record("acme@noolog.io", CHANNEL, "v1");
    first.blog = Some(Blog::with_slug("misty"));
    post(store, CHANNEL, first).await.unwrap()
}

#[tokio::test]
async fn three_revisions_chain_back_to_the_first() {
    let store = MemoryFeedStore::default();
    let v1 = entry(&store).await;
    let v2 = revise(&store, CHANNEL, &v1, v1.clone(), None)
        .await
        .unwrap();
    let v3 = revise(&store, CHANNEL, &v2, v2.clone(), Some("again".into()))
        .await
        .unwrap();

    assert_eq!(v3.revision_number(), 3);
    assert_eq!(v3.previous_revision(), Some(v2.id));
    for (number, id) in [(1, v1.id), (2, v2.id), (3, v3.id)] {
        let found = revision(&store, CHANNEL, v3.clone(), number).await.unwrap();
        assert_eq!(found.map(|r| r.id), Some(id), "revision {number}");
    }
    assert!(
        revision(&store, CHANNEL, v3.clone(), 4)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        get_by_slug(&store, CHANNEL, "misty")
            .await
            .unwrap()
            .map(|r| r.id),
        Some(v3.id)
    );
    let listed: Vec<u64> = page(&store, CHANNEL, None, 10)
        .await
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(listed, vec![v3.id]);
}

#[tokio::test]
async fn an_older_revision_is_refused_with_the_one_to_edit() {
    let store = MemoryFeedStore::default();
    let v1 = entry(&store).await;
    let v2 = revise(&store, CHANNEL, &v1, v1.clone(), None)
        .await
        .unwrap();

    let refused = revise(&store, CHANNEL, &v1, v1.clone(), None)
        .await
        .unwrap_err();
    let stale = refused.downcast_ref::<NotLatest>().expect("NotLatest");
    assert_eq!((stale.id, stale.latest), (v1.id, Some(v2.id)));
    assert_eq!(superseded_by(&store, CHANNEL, v2.id).await.unwrap(), None);
    assert_eq!(
        get_record(&store, CHANNEL, v1.id)
            .await
            .unwrap()
            .map(|r| r.subject),
        Some("v1".into()),
        "the replaced revision stays readable"
    );
}
