//! Push deliveries awaiting a connector's acknowledgement: at most one input row per binding.
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Inflight {
    by_binding: HashMap<String, String>,
}
impl Inflight {
    pub(super) fn is_busy(&self, bid: &str) -> bool {
        self.by_binding.contains_key(bid)
    }
    /// Whether row `rid` is the delivery binding `bid` is waiting on.
    pub(super) fn awaits(&self, bid: &str, rid: &str) -> bool {
        self.by_binding.get(bid).is_some_and(|id| id == rid)
    }
    pub(super) fn start(&mut self, bid: &str, rid: &str) {
        self.by_binding.insert(bid.to_owned(), rid.to_owned());
    }
    /// Stop waiting on binding `bid`, returning the row it was delivering.
    pub(super) fn finish(&mut self, bid: &str) -> Option<String> {
        self.by_binding.remove(bid)
    }
}
