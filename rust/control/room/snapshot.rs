//! The diagnostic snapshot of the room and of one call.
use serde_json::{json, Map, Value};

use super::browsers::{Browser, Target};
use super::Room;

impl Room {
    pub fn snapshot(&self, sid: Option<&str>) -> Value {
        let inner = self.inner.lock().expect("room lock");
        let c = sid.and_then(|s| inner.browsers.get(s));
        let mut utterances:Vec<(u64,Value,Option<Value>)>=inner.utterances.iter().filter_map(|(uid,record)|{
            let row=inner.journal.find(&record.row_id)?;
            let clients:Map<String,Value>=record.clients.iter().map(|(id,(_,status))|(id.clone(),json!(status))).collect();
            let own=sid.and_then(|id|record.clients.get(id)).map(|(_,status)|json!({"utterance_id":uid,"revision":row.revision,"session_id":sid,"status":status}));
            Some((row.seq,json!({"utterance_id":uid,"revision":row.revision,"thread_id":row.thread,"status":row.status,"parked":record.parked,"replay_of":record.replay_of,"clients":clients}),own))
        }).collect();
        utterances.sort_by_key(|entry| entry.0);
        let room_utterances: Vec<Value> = utterances.iter().map(|entry| entry.1.clone()).collect();
        let call_utterances: Vec<Value> =
            utterances.into_iter().filter_map(|entry| entry.2).collect();
        json!({"binding":c.and_then(Browser::bound_target).map(Target::view),
        "room":{"revision":c.map_or(0,|b|b.revision),"speaking":c.is_some_and(|b|b.speaking),"switching":false,"clients":inner.browsers.len(),"utterances":room_utterances,"client_errors":inner.client_errors.view()},
        "clients":inner.browsers.iter().map(|(id,b)|json!({"id":id,"device_id":b.device,"connected":true,"user_speaking":b.speaking,"turn_revision":b.turn_revision})).collect::<Vec<_>>(),
        "call":c.map(|b|json!({"id":sid,"target":b.target.as_ref().map(Target::view).unwrap_or(json!({})),"connected":true,"user_speaking":b.speaking,"error":Value::Null,"sent":b.sent,"last_delivery":Value::Null,"revision":b.revision,"utterances":call_utterances}))})
    }
}
