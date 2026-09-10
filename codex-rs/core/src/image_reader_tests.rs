use super::IMAGE_READER_CACHE_CAPACITY;
use super::IMAGE_READER_INFLIGHT_CAPACITY;
use super::ImageReadRequest;
use super::ImageReaderCache;
use super::ImageReaderCacheKey;
use super::ImageReaderCacheLookup;
use super::reader_text_is_usable;
use crate::config::ViewImageDelegation;
use codex_model_provider_info::WireApi;
use codex_model_provider_info::create_oss_provider_with_base_url;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::Verbosity;
use codex_protocol::models::ImageDetail;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

fn reader() -> ViewImageDelegation {
    ViewImageDelegation {
        model: "reader-model".to_string(),
        provider_id: "openai".to_string(),
        provider: create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses),
        reasoning_effort: ReasoningEffort::Medium,
    }
}

fn request(id: usize) -> ImageReadRequest {
    ImageReadRequest {
        image_url: format!("data:image/png;base64,{id}"),
        detail: Some(ImageDetail::High),
        question: None,
    }
}

fn key(id: usize) -> ImageReaderCacheKey {
    ImageReaderCacheKey::new(
        &reader(),
        &request(id),
        /*model_verbosity*/ None,
        /*model_reasoning_summary*/ None,
        /*service_tier*/ None,
    )
}

#[tokio::test]
async fn cache_coalesces_and_only_caches_successes() {
    let mut cache = ImageReaderCache::default();
    let cache_key = key(1);

    let sender = match cache.begin(cache_key) {
        ImageReaderCacheLookup::Leader(sender) => sender,
        _ => panic!("first request for a new key must lead"),
    };
    let mut waiter = match cache.begin(cache_key) {
        ImageReaderCacheLookup::Wait(receiver) => receiver,
        _ => panic!("same in-flight request must wait"),
    };

    cache.finish(cache_key, sender, Some("description".to_string()));

    assert!(waiter.changed().await.is_ok());
    assert_eq!(waiter.borrow().clone(), Some("description".to_string()));
    assert!(matches!(
        cache.begin(cache_key),
        ImageReaderCacheLookup::Hit(text) if text == "description"
    ));

    let failed_key = key(2);
    let failed_sender = match cache.begin(failed_key) {
        ImageReaderCacheLookup::Leader(sender) => sender,
        _ => panic!("first request for a new key must lead"),
    };
    cache.finish(failed_key, failed_sender, None);
    assert!(matches!(
        cache.begin(failed_key),
        ImageReaderCacheLookup::Leader(_)
    ));
}

#[test]
fn cache_is_bounded_and_evicts_oldest_completed_entry() {
    let mut cache = ImageReaderCache::default();
    for id in 0..=IMAGE_READER_CACHE_CAPACITY {
        let cache_key = key(id);
        let sender = match cache.begin(cache_key) {
            ImageReaderCacheLookup::Leader(sender) => sender,
            _ => panic!("first request for a new key must lead"),
        };
        cache.finish(cache_key, sender, Some(format!("description-{id}")));
    }

    assert!(matches!(
        cache.begin(key(0)),
        ImageReaderCacheLookup::Leader(_)
    ));
    assert!(matches!(
        cache.begin(key(IMAGE_READER_CACHE_CAPACITY)),
        ImageReaderCacheLookup::Hit(_)
    ));
}

/// Regression test for the coalescing deadlock: the old design stored the
/// only `oneshot::Sender` for each waiter inside the shared map, so a leader
/// cancelled before calling `finish` left that sender parked (neither fired
/// nor dropped) and every waiter's `.await` blocked forever.
#[tokio::test]
async fn cancelling_the_leader_wakes_waiters_with_raw_fallback_and_key_is_readable_again() {
    let mut cache = ImageReaderCache::default();
    let cache_key = key(1);

    let sender = match cache.begin(cache_key) {
        ImageReaderCacheLookup::Leader(sender) => sender,
        _ => panic!("first request for a new key must lead"),
    };
    let mut waiter = match cache.begin(cache_key) {
        ImageReaderCacheLookup::Wait(receiver) => receiver,
        _ => panic!("same in-flight request must wait"),
    };

    // Simulates the leader task being cancelled before it ever calls
    // `finish`: dropping its sender is the only signal the cache gets.
    drop(sender);

    assert!(
        waiter.changed().await.is_err(),
        "a cancelled leader must wake waiters instead of hanging forever"
    );

    // The stale entry must not wedge the key forever.
    assert!(matches!(
        cache.begin(cache_key),
        ImageReaderCacheLookup::Leader(_)
    ));
}

#[test]
fn inflight_bound_still_applies_to_live_leaders() {
    let mut cache = ImageReaderCache::default();
    let mut senders = Vec::new();
    for id in 0..IMAGE_READER_INFLIGHT_CAPACITY {
        match cache.begin(key(id)) {
            ImageReaderCacheLookup::Leader(sender) => senders.push(sender),
            _ => panic!("expected a fresh leader below capacity"),
        }
    }

    assert!(matches!(
        cache.begin(key(IMAGE_READER_INFLIGHT_CAPACITY)),
        ImageReaderCacheLookup::Skip
    ));

    cache.finish(key(0), senders.remove(0), None);
    assert!(matches!(
        cache.begin(key(IMAGE_READER_INFLIGHT_CAPACITY)),
        ImageReaderCacheLookup::Leader(_)
    ));
}

/// A cancellation storm across many distinct keys must not permanently wedge
/// the bound: `begin` sweeps closed entries before giving up on a new key,
/// even though none of the cancelled keys were ever individually retried.
#[test]
fn cancelled_leaders_are_swept_when_the_inflight_bound_is_reached() {
    let mut cache = ImageReaderCache::default();
    for id in 0..IMAGE_READER_INFLIGHT_CAPACITY {
        match cache.begin(key(id)) {
            ImageReaderCacheLookup::Leader(sender) => drop(sender),
            _ => panic!("expected a fresh leader below capacity"),
        }
    }

    assert!(matches!(
        cache.begin(key(IMAGE_READER_INFLIGHT_CAPACITY)),
        ImageReaderCacheLookup::Leader(_)
    ));
}

#[test]
fn key_distinguishes_every_field_that_can_change_the_answer() {
    let base_reader = reader();
    let base_request = request(1);
    let base = ImageReaderCacheKey::new(
        &base_reader,
        &base_request,
        /*model_verbosity*/ None,
        /*model_reasoning_summary*/ None,
        /*service_tier*/ None,
    );

    assert_eq!(
        base,
        ImageReaderCacheKey::new(
            &base_reader,
            &base_request,
            /*model_verbosity*/ None,
            /*model_reasoning_summary*/ None,
            /*service_tier*/ None,
        ),
        "identical requests must hash to the same key"
    );

    let mut with_question = request(1);
    with_question.question = Some("what does this say?".to_string());
    assert_ne!(
        base,
        ImageReaderCacheKey::new(
            &base_reader,
            &with_question,
            /*model_verbosity*/ None,
            /*model_reasoning_summary*/ None,
            /*service_tier*/ None,
        )
    );

    let mut with_detail = request(1);
    with_detail.detail = Some(ImageDetail::Low);
    assert_ne!(
        base,
        ImageReaderCacheKey::new(
            &base_reader,
            &with_detail,
            /*model_verbosity*/ None,
            /*model_reasoning_summary*/ None,
            /*service_tier*/ None,
        )
    );

    let mut with_model = base_reader.clone();
    with_model.model = "other-model".to_string();
    assert_ne!(
        base,
        ImageReaderCacheKey::new(
            &with_model,
            &base_request,
            /*model_verbosity*/ None,
            /*model_reasoning_summary*/ None,
            /*service_tier*/ None,
        )
    );

    let mut with_provider = base_reader.clone();
    with_provider.provider_id = "other-provider".to_string();
    assert_ne!(
        base,
        ImageReaderCacheKey::new(
            &with_provider,
            &base_request,
            /*model_verbosity*/ None,
            /*model_reasoning_summary*/ None,
            /*service_tier*/ None,
        )
    );

    let mut with_effort = base_reader.clone();
    with_effort.reasoning_effort = ReasoningEffort::High;
    assert_ne!(
        base,
        ImageReaderCacheKey::new(
            &with_effort,
            &base_request,
            /*model_verbosity*/ None,
            /*model_reasoning_summary*/ None,
            /*service_tier*/ None,
        )
    );

    assert_ne!(
        base,
        ImageReaderCacheKey::new(
            &base_reader,
            &base_request,
            Some(Verbosity::High),
            /*model_reasoning_summary*/ None,
            /*service_tier*/ None,
        )
    );
    assert_ne!(
        base,
        ImageReaderCacheKey::new(
            &base_reader,
            &base_request,
            /*model_verbosity*/ None,
            Some(ReasoningSummary::Detailed),
            /*service_tier*/ None,
        )
    );
    assert_ne!(
        base,
        ImageReaderCacheKey::new(
            &base_reader,
            &base_request,
            /*model_verbosity*/ None,
            /*model_reasoning_summary*/ None,
            Some("flex"),
        )
    );
}

/// Regression test: a stream that ends without a `Completed` event used to
/// still cache whatever partial text had streamed in as long as it was
/// non-empty. There is no proof that text is a full answer, so it must never
/// be treated as usable, let alone cached.
#[test]
fn text_is_rejected_without_a_completed_event_even_when_non_empty() {
    assert!(!reader_text_is_usable(
        /*completed*/ false,
        /*overflowed*/ false,
        "partial but non-empty"
    ));
}

#[test]
fn text_is_rejected_when_overflowed() {
    assert!(!reader_text_is_usable(
        /*completed*/ true,
        /*overflowed*/ true,
        "irrelevant",
    ));
}

#[test]
fn text_is_rejected_when_empty() {
    assert!(!reader_text_is_usable(
        /*completed*/ true, /*overflowed*/ false, "   ",
    ));
}

#[test]
fn text_is_accepted_when_completed_not_overflowed_and_non_empty() {
    assert!(reader_text_is_usable(
        /*completed*/ true,
        /*overflowed*/ false,
        "a real description",
    ));
}
