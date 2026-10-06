use super::{CachedSpeech, SynthesisCache, SynthesisChoice};
use crate::providers::{CloudSpeech, ProviderError, ProviderErrorKind};
use serde_json::{Map, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::oneshot;

fn choice(voice: &str, speed: f64) -> SynthesisChoice<'_> {
    SynthesisChoice {
        place: "elevenlabs",
        model: "eleven_multilingual_v2",
        voice,
        speed,
    }
}

fn speech(byte: u8, len: usize) -> CloudSpeech {
    CloudSpeech {
        audio: vec![byte; len],
        mime_type: "audio/mpeg".to_owned(),
        alignment: None,
        timings_ms: Map::<String, Value>::new(),
    }
}

#[test]
fn key_matches_python_json_dumps_and_separates_synthesis_inputs() {
    assert_eq!(
        SynthesisCache::key(choice("voice-a", 1.0), "hola"),
        "e06a96f582532280a444325bc776c0f1"
    );
    let base = SynthesisCache::key(choice("voice-a", 1.0), "hola");
    for variant in [
        SynthesisCache::key(
            SynthesisChoice {
                place: "other",
                ..choice("voice-a", 1.0)
            },
            "hola",
        ),
        SynthesisCache::key(choice("voice-b", 1.0), "hola"),
        SynthesisCache::key(choice("voice-a", 1.1), "hola"),
        SynthesisCache::key(choice("voice-a", 1.0), "adiós"),
    ] {
        assert_ne!(base, variant);
    }
}

#[tokio::test]
async fn concurrent_misses_share_one_render_and_only_one_waiter_is_fresh() {
    let cache = Arc::new(SynthesisCache::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let render_calls = Arc::clone(&calls);
    let render = move || async move {
        render_calls.fetch_add(1, Ordering::SeqCst);
        tokio::task::yield_now().await;
        Ok(speech(7, 4))
    };

    let (first, second) = tokio::join!(
        cache.obtain(choice("voice-a", 1.0), "hola", render),
        cache.obtain(choice("voice-a", 1.0), "hola", || async {
            Ok(speech(9, 4))
        }),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_ne!(first.fresh, second.fresh);
    assert!(Arc::ptr_eq(&first.speech, &second.speech));
    assert_eq!(cache.stats().renders, 1);
    assert_eq!(cache.stats().reuses, 1);
}

#[tokio::test]
async fn cancelling_a_waiter_does_not_cancel_the_shared_render() {
    let cache = Arc::new(SynthesisCache::new());
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let first_cache = Arc::clone(&cache);
    let first = tokio::spawn(async move {
        first_cache
            .obtain(choice("voice-a", 1.0), "hola", move || async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok(speech(4, 4))
            })
            .await
    });
    started_rx.await.unwrap();
    let second_cache = Arc::clone(&cache);
    let second = tokio::spawn(async move {
        second_cache
            .obtain(choice("voice-a", 1.0), "hola", || async {
                Ok(speech(8, 4))
            })
            .await
    });
    while cache.stats().reuses == 0 {
        tokio::task::yield_now().await;
    }
    first.abort();
    let _ = release_tx.send(());
    let received = second.await.unwrap().unwrap();
    assert!(!received.fresh);
    assert_eq!(received.speech.audio, vec![4; 4]);
    assert_eq!(cache.stats().renders, 1);
}

#[tokio::test]
async fn cache_hit_read_lru_and_base64_byte_limit_match_python_behavior() {
    let cache = SynthesisCache::with_limits(2, 64);
    let first: CachedSpeech = cache
        .obtain(choice("voice-a", 1.0), "one", || async { Ok(speech(1, 4)) })
        .await
        .unwrap();
    let _ = cache
        .obtain(choice("voice-a", 1.0), "two", || async { Ok(speech(2, 4)) })
        .await
        .unwrap();
    let key_one = SynthesisCache::key(choice("voice-a", 1.0), "one");
    let read = cache.read(&key_one).unwrap();
    assert!(Arc::ptr_eq(&read, &first.speech));
    let _ = cache
        .obtain(choice("voice-a", 1.0), "three", || async {
            Ok(speech(3, 4))
        })
        .await
        .unwrap();
    assert!(cache.read(&key_one).is_some());
    assert!(cache
        .read(&SynthesisCache::key(choice("voice-a", 1.0), "two"))
        .is_none());
    assert_eq!(cache.stats().items, 2);
    assert_eq!(cache.stats().bytes, 16); // Each 4-byte result encodes to 8 Base64 bytes.

    let byte_limited = SynthesisCache::with_limits(8, 9);
    let _ = byte_limited
        .obtain(choice("voice-a", 1.0), "four", || async {
            Ok(speech(1, 4))
        })
        .await
        .unwrap();
    let _ = byte_limited
        .obtain(choice("voice-a", 1.0), "five", || async {
            Ok(speech(2, 1))
        })
        .await
        .unwrap();
    assert_eq!(byte_limited.stats().items, 1);
    assert_eq!(byte_limited.stats().bytes, 4);
}

#[tokio::test]
async fn failed_render_is_not_cached_and_does_not_increment_successful_render_count() {
    let cache = SynthesisCache::new();
    let error = ProviderError::new(ProviderErrorKind::RateLimited, Some(429));
    assert_eq!(
        cache
            .obtain(
                choice("voice-a", 1.0),
                "hola",
                move || async move { Err(error) }
            )
            .await
            .unwrap_err()
            .kind,
        ProviderErrorKind::RateLimited
    );
    assert_eq!(cache.stats().items, 0);
    assert_eq!(cache.stats().renders, 0);
    let _ = cache
        .obtain(choice("voice-a", 1.0), "hola", || async {
            Ok(speech(1, 1))
        })
        .await
        .unwrap();
    assert_eq!(cache.stats().renders, 1);
}
