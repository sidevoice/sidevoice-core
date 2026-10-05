//! A small bounded cache for paid cloud speech shared across listeners.
//!
//! The cache stores provider results, but owns no provider client, room state, or receipt state. Callers
//! supply a render future so cancellation of one caller cannot cancel a render shared with other callers.

use super::{CloudSpeech, ProviderError, ProviderErrorKind};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
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

type RenderResult = Option<Result<Arc<CloudSpeech>, ProviderError>>;
type RenderSender = watch::Sender<RenderResult>;

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    lru: VecDeque<String>,
    inflight: HashMap<String, RenderSender>,
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
        digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Returns a cached value and marks it most-recently-used, without changing reuse counters.
    pub fn read(&self, key: &str) -> Option<Arc<CloudSpeech>> {
        let mut state = self.inner.lock();
        let speech = state
            .entries
            .get(key)
            .map(|entry| Arc::clone(&entry.speech));
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
            let mut state = self.inner.lock();
            if let Some(speech) = state
                .entries
                .get(&key)
                .map(|entry| Arc::clone(&entry.speech))
            {
                touch(&mut state.lru, &key);
                state.reuses += 1;
                return Ok(CachedSpeech {
                    speech,
                    fresh: false,
                });
            }

            if let Some(receiver) = state.inflight.get(&key).map(watch::Sender::subscribe) {
                state.reuses += 1;
                (receiver, false, None)
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
                return Err(ProviderError::new(ProviderErrorKind::Transport, None));
            }
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result.map(|speech| CachedSpeech { speech, fresh });
            }
        }
    }

    /// Returns the observable cache accounting used by the Python implementation.
    pub fn stats(&self) -> SynthesisCacheStats {
        let state = self.inner.lock();
        SynthesisCacheStats {
            items: state.entries.len(),
            bytes: state.bytes,
            renders: state.renders,
            reuses: state.reuses,
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("synthesis cache mutex poisoned")
    }

    fn store(&self, key: String, speech: Arc<CloudSpeech>) {
        let encoded_audio_bytes = speech.audio.len().div_ceil(3) * 4;
        let mut state = self.lock();
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
        self.lock().inflight.remove(key);
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
mod tests;
