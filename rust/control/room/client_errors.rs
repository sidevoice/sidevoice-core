//! Errors browsers report about themselves, kept briefly for diagnosis.
use std::collections::VecDeque;

use serde_json::{json, Value};

use super::util::{field, millis};
use super::Room;

const MAX_CLIENT_ERRORS: usize = 20;

#[derive(Default)]
pub(super) struct ClientErrors {
    recent: VecDeque<Value>,
}
impl ClientErrors {
    fn record(&mut self, report: &Value) {
        let clipped =
            |key: &str, max: usize| field(report, key).chars().take(max).collect::<String>();
        self.recent.push_back(json!({"session_id":clipped("session_id",100),"kind":clipped("kind",40),"message":clipped("message",200),"at":millis()}));
        while self.recent.len() > MAX_CLIENT_ERRORS {
            self.recent.pop_front();
        }
    }
    pub(super) fn view(&self) -> Value {
        json!(self.recent)
    }
}

impl Room {
    pub fn report_client_error(&self, report: &Value) -> Value {
        self.inner
            .lock()
            .expect("room lock")
            .client_errors
            .record(report);
        json!({"status":"recorded"})
    }
}
