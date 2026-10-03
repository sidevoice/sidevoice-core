//! A small bounded cache for paid cloud speech shared across listeners.
//!
//! The cache stores provider results, but owns no provider client, room state, or receipt state. Callers
//! supply a render future so cancellation of one caller cannot cancel a render shared with other callers.

use super::{CloudSpeech, ProviderError};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

const DEFAULT_MAX_ITEMS: usize = 64;
const DEFAULT_MAX_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct SynthesisChoice<'a> {
    pub place: &'a str,
    pub model: &'a str,
    pub voice: &'a str,
    pub speed: f64,
}

#[derive(Clone, Debug)]
pub struct CachedSpeech {
    pub speech: Arc<CloudSpeech>,
    pub fresh: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SynthesisCacheStats {
    pub items: usize,
    pub bytes: usize,
    pub renders: u64,
    pub reuses: u64,
}

pub struct SynthesisCache {
    inner: Arc<Inner>,
}

struct Inner {
    max_items: usize,
    max_bytes: usize,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    lru: VecDeque<String>,
    inflight: HashMap<String, watch::Sender<Option<Result<Arc<CloudSpeech>, ProviderError>>>>,
    bytes: usize,
    renders: u64,
    reuses: u64,
}

struct Entry {
    speech: Arc<CloudSpeech>,
    encoded_audio_bytes: usize,
}

impl Default for SynthesisCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SynthesisCache {
    /// Creates a cache with the pinned Python limits: 64 entries and 32 MiB encoded audio.
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_ITEMS, DEFAULT_MAX_BYTES)
    }

    fn with_limits(max_items: usize, max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                max_items,
                max_bytes,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Stable first 32 lowercase SHA-256 hex characters over Python's JSON array key shape.
    pub fn key(choice: SynthesisChoice<'_>, text: &str) -> String {
        // Python's json.dumps defaults to a space after each array comma. Preserve those spaces as
        // they are part of the existing cache key, while serde_json provides matching UTF-8 escaping.
        let material = format!(
            "[{}, {}, {}, {}, {}]",
            serde_json::to_string(choice.place).expect("serializing a string cannot fail"),
            serde_json::to_string(choice.model).expect("serializing a string cannot fail"),
            serde_json::to_string(choice.voice).expect("serializing a string cannot fail"),
            serde_json::to_string(&choice.speed).expect("serializing a finite speed cannot fail"),
            serde_json::to_string(text).expect("serializing a string cannot fail"),
        );
        let digest = Sha256::digest(material.as_bytes());
        digest[..16].iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// Returns a cached value and marks it most-recently-used, without changing reuse counters.
    pub fn read(&self, key: &str) -> Option<Arc<CloudSpeech>> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("synthesis cache mutex poisoned");
        let speech = state.entries.get(key).map(|entry| Arc::clone(&entry.speech));
        if speech.is_some() {
            touch(&mut state.lru, key);
        }
        speech
    }

    /// Returns a cached render or joins/starts exactly one in-flight render for this synthesis key.
    ///
    /// The first caller receives `fresh = true`; a cache hit or in-flight join receives `false`. A
    /// cancelled waiter drops only its receiver. The spawned render continues and can fill the cache.
    pub async fn obtain<F, Fut>(
        &self,
        choice: SynthesisChoice<'_>,
        text: &str,
        render: F,
    ) -> Result<CachedSpeech, ProviderError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<CloudSpeech, ProviderError>> + Send + 'static,
    {
        let key = Self::key(choice, text);
        let (mut receiver, fresh, start_render) = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("synthesis cache mutex poisoned");
            if let Some(speech) = state.entries.get(&key).map(|entry| Arc::clone(&entry.speech)) {
                touch(&mut state.lru, &key);
                state.reuses += 1;
                return Ok(CachedSpeech {
                    speech,
                    fresh: false,
                });
            }

            if let Some(sender) = state.inflight.get(&key) {
                state.reuses += 1;
                (sender.subscribe(), false, None)
            } else {
                let (sender, receiver) = watch::channel(None);
                state.inflight.insert(key.clone(), sender.clone());
                (receiver, true, Some((render, sender)))
            }
        };

        if let Some((render, sender)) = start_render {
            let task_key = key.clone();
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move {
                let result = render().await.map(Arc::new);
                if let Ok(speech) = &result {
                    inner.store(task_key.clone(), Arc::clone(speech));
                }
                inner.finish(&task_key);
                let _ = sender.send(Some(result));
            });
        }

        loop {
            if receiver.changed().await.is_err() {
                return Err(ProviderError::new(
                    super::ProviderErrorKind::Transport,
                    None,
                ));
            }
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result.map(|speech| CachedSpeech { speech, fresh });
            }
        }
    }

    /// Returns the observable cache accounting used by the Python implementation.
    pub fn stats(&self) -> SynthesisCacheStats {
        let state = self
            .inner
            .state
            .lock()
            .expect("synthesis cache mutex poisoned");
        SynthesisCacheStats {
            items: state.entries.len(),
            bytes: state.bytes,
            renders: state.renders,
            reuses: state.reuses,
        }
    }
}

impl Inner {
    fn store(&self, key: String, speech: Arc<CloudSpeech>) {
        let encoded_audio_bytes = speech.audio.len().div_ceil(3) * 4;
        let mut state = self.state.lock().expect("synthesis cache mutex poisoned");
        state.renders += 1;
        if let Some(previous) = state.entries.remove(&key) {
            state.bytes -= previous.encoded_audio_bytes;
            remove_lru(&mut state.lru, &key);
        }
        state.bytes += encoded_audio_bytes;
        state.entries.insert(
            key.clone(),
            Entry {
                speech,
                encoded_audio_bytes,
            },
        );
        state.lru.push_back(key);

        while !state.entries.is_empty()
            && (state.entries.len() > self.max_items || state.bytes > self.max_bytes)
        {
            let oldest = state.lru.pop_front().expect("every entry has an LRU key");
            if let Some(entry) = state.entries.remove(&oldest) {
                state.bytes -= entry.encoded_audio_bytes;
            }
        }
    }

    fn finish(&self, key: &str) {
        self.state
            .lock()
            .expect("synthesis cache mutex poisoned")
            .inflight
            .remove(key);
    }
}

fn touch(lru: &mut VecDeque<String>, key: &str) {
    remove_lru(lru, key);
    lru.push_back(key.to_owned());
}

fn remove_lru(lru: &mut VecDeque<String>, key: &str) {
    if let Some(position) = lru.iter().position(|item| item == key) {
        lru.remove(position);
    }
}

#[cfg(test)]
mod tests {
    use super::{CachedSpeech, SynthesisCache, SynthesisChoice};
    use crate::providers::{
        CloudSpeech, ProviderError, ProviderErrorKind,
    };
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
            cache.obtain(choice("voice-a", 1.0), "hola", || async { Ok(speech(9, 4)) }),
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
                .obtain(choice("voice-a", 1.0), "hola", || async { Ok(speech(8, 4)) })
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
            .obtain(choice("voice-a", 1.0), "three", || async { Ok(speech(3, 4)) })
            .await
            .unwrap();
        assert!(cache.read(&key_one).is_some());
        assert!(cache
            .read(&SynthesisCache::key(choice("voice-a", 1.0), "two"))
            .is_none());
        assert_eq!(cache.stats().items, 2);
        assert_eq!(cache.stats().bytes, 24); // 4 raw bytes encode to 8 Base64 bytes per entry.

        let byte_limited = SynthesisCache::with_limits(8, 9);
        let _ = byte_limited
            .obtain(choice("voice-a", 1.0), "four", || async { Ok(speech(1, 4)) })
            .await
            .unwrap();
        let _ = byte_limited
            .obtain(choice("voice-a", 1.0), "five", || async { Ok(speech(2, 1)) })
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
                .obtain(choice("voice-a", 1.0), "hola", move || async move {
                    Err(error)
                })
                .await
                .unwrap_err()
                .kind,
            ProviderErrorKind::RateLimited
        );
        assert_eq!(cache.stats().items, 0);
        assert_eq!(cache.stats().renders, 0);
        let _ = cache
            .obtain(choice("voice-a", 1.0), "hola", || async { Ok(speech(1, 1)) })
            .await
            .unwrap();
        assert_eq!(cache.stats().renders, 1);
    }
}
