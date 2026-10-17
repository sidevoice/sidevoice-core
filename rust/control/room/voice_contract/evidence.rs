//! Conservative app evidence and model-facing physical deltas.
use super::*;

impl Inner {
    pub(in crate::control::room) fn verified_snapshot(
        &self,
        thread: &str,
        sid: &str,
        value: &Value,
    ) -> Value {
        let value = validated_snapshot(value);
        if value["knowledge"] != "known" {
            return value;
        }
        let valid = |item: &Value| {
            let uid = field(item, "utterance_id");
            let Some(original) = self
                .voice_contract
                .originals
                .get(uid)
                .filter(|o| o.thread == thread)
            else {
                return false;
            };
            let length = original.text.chars().count() as u64;
            let extent_is_valid = match field(item, "hearing") {
                "complete" => item["heard_chars"].as_u64() == Some(length),
                "partial" => item["heard_chars"]
                    .as_u64()
                    .is_some_and(|n| n > 0 && n < length),
                "not_played" => item["heard_chars"] == 0,
                "unknown" => item["heard_chars"].is_null(),
                _ => false,
            };
            extent_is_valid
                && self.voice_contract.attempts.values().any(|a| {
                    a.original == uid && a.listeners.iter().any(|(listener, _)| listener == sid)
                })
        };
        if !value["recent"]
            .as_array()
            .is_some_and(|recent| recent.iter().all(valid))
            || (!value["last_played"].is_null()
                && (!valid(&value["last_played"])
                    || value["last_played"]["hearing"] == "not_played"))
        {
            return validated_snapshot(&Value::Null);
        }
        value
    }

    pub(in crate::control::room) fn verified_not_played(
        &self,
        thread: &str,
        sid: &str,
        value: &Value,
    ) -> Value {
        let (items, _) = validated_not_played(value);
        json!(items
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| {
                let uid = field(item, "utterance_id");
                self.voice_contract
                    .originals
                    .get(uid)
                    .is_some_and(|o| o.thread == thread)
                    && self.voice_contract.attempts.values().any(|a| {
                        a.original == uid && a.listeners.iter().any(|(listener, _)| listener == sid)
                    })
            })
            .cloned()
            .collect::<Vec<_>>())
    }

    pub(in crate::control::room) fn contract_input_facts(
        &mut self,
        thread: &str,
        payload: &mut Value,
    ) {
        payload["hearing_changes"] = json!(self
            .voice_contract
            .changes
            .remove(thread)
            .unwrap_or_default()
            .into_iter()
            .map(|change| change.fact)
            .collect::<Vec<_>>());
        payload["playback_outcomes"] = json!(self
            .voice_contract
            .outcomes
            .remove(thread)
            .unwrap_or_default());
        if self.voice_contract.changes_truncated.remove(thread) {
            payload["hearing_changes_truncated"] = json!(true);
        }
        if self.voice_contract.outcomes_truncated.remove(thread) {
            payload["playback_outcomes_truncated"] = json!(true);
        }
    }
}

/// A conservative sentence start in Unicode scalar indices. Decimal dots and common short
/// abbreviations are not separators; quotes and line breaks after punctuation are skipped.
pub(in crate::control::room) fn sentence_start(text: &str, confirmed: u64) -> u64 {
    let chars: Vec<char> = text.chars().collect();
    let end = (confirmed as usize).min(chars.len());
    let mut start = 0;
    for index in 0..end {
        let ch = chars[index];
        if !matches!(ch, '.' | '?' | '!' | '…') {
            continue;
        }
        if ch == '.'
            && index > 0
            && index + 1 < chars.len()
            && chars[index - 1].is_ascii_digit()
            && chars[index + 1].is_ascii_digit()
        {
            continue;
        }
        if ch == '.' {
            let word: String = chars[..index]
                .iter()
                .rev()
                .take_while(|c| c.is_alphabetic())
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            if [
                "Mr", "Mrs", "Ms", "Dr", "Prof", "Sr", "Jr", "vs", "etc", "e", "g", "i",
            ]
            .contains(&word.as_str())
            {
                continue;
            }
            if index + 1 < chars.len() && chars[index + 1] == '.' {
                continue;
            }
        }
        let mut next = index + 1;
        while next < chars.len()
            && matches!(
                chars[next],
                '.' | '"' | '\'' | '”' | '’' | ')' | ']' | '\n' | ' ' | '\t'
            )
        {
            next += 1;
        }
        if next <= end {
            start = next;
        }
    }
    start as u64
}

pub(in crate::control::room) fn validated_snapshot(value: &Value) -> Value {
    let Some(knowledge) = value.get("knowledge").and_then(Value::as_str) else {
        return json!({"knowledge":"unknown","last_played":null,"recent":[],"truncated":false});
    };
    if knowledge == "unknown" {
        return json!({"knowledge":"unknown","last_played":null,"recent":[],"truncated":false});
    }
    if knowledge != "known" {
        return json!({"knowledge":"unknown","last_played":null,"recent":[],"truncated":false});
    }
    let Some(recent) = value.get("recent").and_then(Value::as_array) else {
        return json!({"knowledge":"unknown","last_played":null,"recent":[],"truncated":false});
    };
    if recent.len() > 8 || !recent.iter().all(valid_hearing_item) {
        return json!({"knowledge":"unknown","last_played":null,"recent":[],"truncated":false});
    }
    let last = value.get("last_played").cloned().unwrap_or(Value::Null);
    if !last.is_null() && !valid_hearing_item(&last) {
        return json!({"knowledge":"unknown","last_played":null,"recent":[],"truncated":false});
    }
    json!({"knowledge":"known","last_played":last,"recent":recent,
        "truncated":value["truncated"].as_bool().unwrap_or(false)})
}

fn valid_hearing_item(value: &Value) -> bool {
    if !valid_id(field(value, "utterance_id")) {
        return false;
    }
    match field(value, "hearing") {
        "unknown" => value.get("heard_chars").is_none_or(Value::is_null),
        "complete" | "partial" => value
            .get("heard_chars")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0),
        "not_played" => value["heard_chars"] == 0,
        _ => false,
    }
}

pub(in crate::control::room) fn validated_not_played(value: &Value) -> (Value, bool) {
    let Some(items) = value.as_array() else {
        return (json!([]), false);
    };
    let valid: Vec<Value> = items
        .iter()
        .filter(|item| {
            field(item, "hearing") == "not_played"
                && item["heard_chars"] == 0
                && valid_id(field(item, "utterance_id"))
        })
        .cloned()
        .collect();
    let clipped = valid.len() > 8;
    let latest = valid
        .into_iter()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>();
    (json!(latest), clipped)
}
