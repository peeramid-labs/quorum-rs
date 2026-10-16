//! The behaviour every [`FeedStore`] must show, written once and run against
//! each store: the in-memory reference and JetStream here, PostgreSQL in the
//! node core. Each check takes a store and an address no earlier run has used.

#![allow(clippy::missing_errors_doc)]

use anyhow::Result;

use crate::*;

pub fn a_channel(address: &str) -> Channel {
    Channel {
        address: address.to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    }
}

pub fn a_record(from: &str, to: &str, subject: &str) -> Record {
    Record {
        id: 0,
        thread_id: None,
        from: from.to_string(),
        to: vec![to.to_string()],
        subject: subject.to_string(),
        received_at: chrono::Utc::now().to_rfc3339(),
        size: subject.len() as u64,
        keywords: Vec::new(),
        text_body: Some(format!("the body of {subject}")),
        html_body: None,
        attachments: Vec::new(),
        signed_by: None,
        message_id: None,
        in_reply_to: None,
        references: Vec::new(),
        blog: None,
    }
}

/// Mail threads by `Message-Id`, and a record mirrored from mail must be
/// findable by it — so a reply's `In-Reply-To` lands in the right thread.
/// The index is create-only: a redelivered message maps to the record it
/// already made rather than to a duplicate.
pub async fn a_message_id_names_the_record_it_arrived_as_and_only_the_first(
    store: &dyn FeedStore,
    address: &str,
) -> Result<()> {
    ensure_channel(store, &a_channel(address)).await?;
    assert!(
        get_by_message_id(store, address, "<a@x>").await?.is_none(),
        "nothing indexed yet"
    );

    let mut first = a_record("acme@noolog.io", address, "patch");
    first.message_id = Some("<a@x>".to_string());
    first.references = vec!["<cover@x>".to_string()];
    let first = post(store, address, first).await?;
    let found = get_by_message_id(store, address, "<a@x>")
        .await?
        .expect("indexed");
    assert_eq!(found.id, first.id);
    assert_eq!(found.references, vec!["<cover@x>".to_string()]);

    // The same message again — a redelivery — is a new record in the
    // stream, but the id still names the first.
    let mut again = a_record("acme@noolog.io", address, "patch again");
    again.message_id = Some("<a@x>".to_string());
    let again = post(store, address, again).await?;
    assert_ne!(again.id, first.id);
    assert_eq!(
        get_by_message_id(store, address, "<a@x>")
            .await?
            .map(|r| r.id),
        Some(first.id)
    );
    Ok(())
}

pub async fn a_slug_names_the_newest_revision_and_older_ones_keep_their_ids(
    store: &dyn FeedStore,
    address: &str,
) -> Result<()> {
    ensure_channel(store, &a_channel(address)).await?;
    assert!(slug_entry(store, address, "entry").await?.is_none());
    assert!(get_by_slug(store, address, "entry").await?.is_none());

    let mut first = a_record("acme@noolog.io", address, "v1");
    first.blog = Some(crate::blog::Blog::with_slug("entry"));
    let first = post(store, address, first).await?;
    assert_eq!(
        slug_entry(store, address, "entry").await?,
        Some(SlugEntry {
            id: first.id,
            from: "acme@noolog.io".into()
        })
    );

    let mut second = a_record("acme@noolog.io", address, "v2");
    second.blog = Some(crate::blog::Blog::with_slug("entry"));
    let second = post(store, address, second).await?;

    let current = get_by_slug(store, address, "entry")
        .await?
        .expect("indexed");
    assert_eq!(current.subject, "v2", "the slug follows the revision");
    assert_eq!(current.id, second.id);
    assert_eq!(
        get_record(store, address, first.id)
            .await?
            .map(|r| r.subject),
        Some("v1".into()),
        "the earlier revision is still there by id"
    );
    // A draft revision is stored but not indexed: the slug keeps serving
    // the live revision while the next one is being written.
    let mut draft = a_record("acme@noolog.io", address, "v3-draft");
    draft.blog = Some(crate::blog::Blog::with_slug("entry"));
    draft.keywords = vec![DRAFT.to_string()];
    let draft = post(store, address, draft).await?;
    assert!(get_record(store, address, draft.id).await?.is_none());
    assert_eq!(
        get_by_slug(store, address, "entry")
            .await?
            .map(|r| r.subject),
        Some("v2".into()),
        "a draft does not take the live entry off the air"
    );
    Ok(())
}

pub async fn a_slug_cannot_be_taken_from_another_operator_even_in_a_race(
    store: &dyn FeedStore,
    address: &str,
) -> Result<()> {
    ensure_channel(store, &a_channel(address)).await?;

    // Both pass any check made before writing, since neither has written.
    let mut acme = a_record("acme@noolog.io", address, "acme's");
    acme.blog = Some(crate::blog::Blog::with_slug("entry"));
    let mut rival = a_record("rival@noolog.io", address, "rival's");
    rival.blog = Some(crate::blog::Blog::with_slug("entry"));
    let (a, b) = tokio::join!(post(store, address, acme), post(store, address, rival));

    let (winner, loser) = match (a, b) {
        (Ok(w), Err(l)) | (Err(l), Ok(w)) => (w, l),
        (Ok(_), Ok(_)) => panic!("both took the slug"),
        (Err(a), Err(b)) => panic!("neither took the slug: {a:#} / {b:#}"),
    };
    let lost = loser
        .downcast_ref::<SlugTaken>()
        .expect("the loser learns whose slug it is");
    assert_eq!(lost.held_by, winner.from);
    assert_eq!(
        get_by_slug(store, address, "entry").await?.map(|r| r.from),
        Some(winner.from.clone())
    );

    // A later attempt by the loser is refused the same way, and the
    // record it wrote stays reachable by id — nothing is rolled back.
    let loser_from = if winner.from == "acme@noolog.io" {
        "rival@noolog.io"
    } else {
        "acme@noolog.io"
    };
    let mut again = a_record(loser_from, address, "once more");
    again.blog = Some(crate::blog::Blog::with_slug("entry"));
    let refused = post(store, address, again).await.unwrap_err();
    assert!(refused.downcast_ref::<SlugTaken>().is_some());
    assert_eq!(
        get_by_slug(store, address, "entry").await?.map(|r| r.from),
        Some(winner.from)
    );
    Ok(())
}

pub async fn records_from_an_id_come_back_oldest_first_and_bounded(
    store: &dyn FeedStore,
    address: &str,
) -> Result<()> {
    ensure_channel(store, &a_channel(address)).await?;
    let mut ids = Vec::new();
    for n in 1..=5 {
        let posted = post(
            store,
            address,
            a_record("test-op@noolog.io", address, &format!("post {n}")),
        )
        .await?;
        ids.push(posted.id);
    }
    let from_third = page_from(store, address, ids[2], 10).await?;
    assert_eq!(
        from_third
            .iter()
            .map(|r| r.subject.as_str())
            .collect::<Vec<_>>(),
        vec!["post 3", "post 4", "post 5"]
    );
    assert_eq!(
        page_from(store, address, ids[0], 2).await?.len(),
        2,
        "at most `limit`"
    );
    assert!(
        page_from(store, "nobody@noolog.io", 1, 10)
            .await?
            .is_empty()
    );
    Ok(())
}

pub async fn records_come_back_newest_first_and_page_by_the_last_id_seen(
    store: &dyn FeedStore,
    address: &str,
) -> Result<()> {
    ensure_channel(store, &a_channel(address)).await?;

    for n in 1..=5 {
        post(
            store,
            address,
            a_record("test-op@noolog.io", address, &format!("post {n}")),
        )
        .await?;
    }

    let newest = page(store, address, None, 2).await?;
    assert_eq!(
        newest
            .iter()
            .map(|r| r.subject.as_str())
            .collect::<Vec<_>>(),
        vec!["post 5", "post 4"],
        "a feed reads newest first"
    );
    assert!(newest[0].id > newest[1].id, "ids order the same way");

    let next = page(store, address, Some(newest[1].id), 2).await?;
    assert_eq!(
        next.iter().map(|r| r.subject.as_str()).collect::<Vec<_>>(),
        vec!["post 3", "post 2"],
        "paging by the last id seen continues without repeating"
    );

    let tail = page(store, address, Some(next[1].id), 10).await?;
    assert_eq!(tail.len(), 1, "the last page is short, not an error");
    assert_eq!(tail[0].subject, "post 1");
    Ok(())
}

pub async fn an_id_is_assigned_by_the_channel_not_the_caller(
    store: &dyn FeedStore,
    address: &str,
) -> Result<()> {
    // A caller-chosen id would let one post overwrite another's permalink.
    ensure_channel(store, &a_channel(address)).await?;

    let mut claimed = a_record("test-op@noolog.io", address, "mine");
    claimed.id = 9_999;
    let posted = post(store, address, claimed).await?;
    assert_ne!(posted.id, 9_999, "the caller's id must be ignored");

    let fetched = get_record(store, address, posted.id)
        .await?
        .expect("stored");
    assert_eq!(fetched.subject, "mine");
    assert_eq!(fetched.id, posted.id, "the permalink is the assigned id");
    Ok(())
}

pub async fn a_draft_is_stored_but_never_served(
    store: &dyn FeedStore,
    address: &str,
) -> Result<()> {
    ensure_channel(store, &a_channel(address)).await?;

    let mut draft = a_record("test-op@noolog.io", address, "unfinished");
    draft.keywords = vec![DRAFT.to_string()];
    let draft = post(store, address, draft).await?;
    post(
        store,
        address,
        a_record("test-op@noolog.io", address, "published"),
    )
    .await?;

    let visible = page(store, address, None, 10).await?;
    assert_eq!(
        visible
            .iter()
            .map(|r| r.subject.as_str())
            .collect::<Vec<_>>(),
        vec!["published"],
        "a draft is for its author, not its readers"
    );
    assert!(
        get_record(store, address, draft.id).await?.is_none(),
        "nor is it reachable by permalink"
    );
    Ok(())
}

pub async fn anybody_may_post_to_a_channel_and_the_first_use_registers_it(
    store: &dyn FeedStore,
    address: &str,
) -> Result<()> {
    // A channel is an address, not a resource. Two operators writing to the
    // same one is the ordinary case — it is how a mailing list works, and
    // the reason there is nothing to take over.

    let first = ensure_channel(store, &a_channel(address)).await?;
    let again = ensure_channel(store, &a_channel(address)).await?;
    assert_eq!(first.address, again.address);
    // The date is when the address was first used, so a later poster must
    // not move it forward.
    assert_eq!(
        first.created_at,
        get_channel(store, address)
            .await?
            .expect("registered")
            .created_at
    );

    post(store, address, a_record("alice@noolog.io", address, "hers")).await?;
    post(store, address, a_record("bob@noolog.io", address, "his")).await?;
    let page = page(store, address, None, 10).await?;
    let senders: Vec<&str> = page.iter().map(|r| r.from.as_str()).collect();
    assert!(
        senders.contains(&"alice@noolog.io") && senders.contains(&"bob@noolog.io"),
        "both belong in the same channel: {senders:?}"
    );
    Ok(())
}

pub async fn an_empty_channel_is_an_empty_page_not_a_failure(
    store: &dyn FeedStore,
    address: &str,
) -> Result<()> {
    ensure_channel(store, &a_channel(address)).await?;
    assert!(page(store, address, None, 10).await?.is_empty());

    // And a channel nobody registered reads as empty rather than erroring.
    assert!(
        page(store, "test-feed-absent@noolog.io", None, 10)
            .await?
            .is_empty()
    );
    Ok(())
}

pub async fn the_index_lists_registered_channels(
    store: &dyn FeedStore,
    address: &str,
) -> Result<()> {
    ensure_channel(store, &a_channel(address)).await?;

    let listed = list_channels(store).await?;
    assert!(
        listed.iter().any(|c| c.address == address),
        "a registered channel must appear in the index"
    );
    Ok(())
}
