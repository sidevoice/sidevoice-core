//! The core's errors as logs name them (`docs/logging.md`, section 6): one enum, one stable code per variant.
//!
//! `Display` is for people and callers and is never logged; a log line carries `error.type = error.code()`. The
//! wire keys the core answers with (`room.focus_changed`, `bind.core-running`) are a protocol and stay as they are;
//! this enum maps each to its code, so a code can be renamed without touching the wire.

/// What went wrong, at the granularity a person debugging a log needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    #[error("the log filter is not valid")]
    LogFilterInvalid,
    #[error("the core could not start")]
    StartFailed,
    #[error("another core holds the data directory")]
    AlreadyRunning,
    #[error("the port is taken")]
    PortInUse,
    #[error("the data directory is not private")]
    UnsafeDirectory,
    #[error("the node identity cannot be read")]
    IdentityUnreadable,
    #[error("a listener stopped after the core was ready")]
    Crashed,
    #[error("too many attempts")]
    RateLimited,
    #[error("too many connections")]
    ConnectionLimit,
    #[error("the pairing secret was refused")]
    PairingRefused,
    #[error("the paired device is unknown")]
    DeviceUnknown,
    #[error("the state could not be saved")]
    StateSaveFailed,
    #[error("the binding is not known")]
    BindingUnknown,
    /// A refusal the core answers with a wire key (a room operation, a speech publication).
    #[error("the request was refused")]
    Refused(&'static str),
}

impl CoreError {
    /// The stable `<component>.<kebab-name>` code; the value of `error.type`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::LogFilterInvalid => "core.log-filter-invalid",
            Self::StartFailed => "core.start-failed",
            Self::AlreadyRunning => "core.already-running",
            Self::PortInUse => "core.port-in-use",
            Self::UnsafeDirectory => "core.unsafe-directory",
            Self::IdentityUnreadable => "core.identity-unreadable",
            Self::Crashed => "core.crashed",
            Self::RateLimited => "core.rate-limited",
            Self::ConnectionLimit => "core.connection-limit",
            Self::PairingRefused => "core.pairing-refused",
            Self::DeviceUnknown => "core.device-unknown",
            Self::StateSaveFailed => "core.state-save-failed",
            Self::BindingUnknown => "core.binding-unknown",
            Self::Refused(key) => refusal_code(key),
        }
    }

    /// The error a launch failure's wire key stands for; anything unknown is a plain start failure.
    pub fn from_start_key(key: &str) -> Self {
        match key {
            "bind.core-running" => Self::AlreadyRunning,
            "bind.port-in-use" => Self::PortInUse,
            "identity.unsafe-directory" => Self::UnsafeDirectory,
            "identity.unreadable" => Self::IdentityUnreadable,
            _ => Self::StartFailed,
        }
    }
}

/// The code of a refusal's wire key. A key that is not listed here is `core.refused`, so a key is never logged as
/// itself (one may come from a peer).
pub fn refusal_code(key: &str) -> &'static str {
    match key {
        "room.binding_closed" => "core.binding-closed",
        "room.binding_foreign" => "core.binding-foreign",
        "room.binding_thread_mismatch" => "core.binding-thread-mismatch",
        "room.browser_absent" => "core.browser-absent",
        "room.connector_disconnected" => "core.connector-disconnected",
        "room.focus_changed" => "core.focus-changed",
        "room.full" => "core.room-full",
        "room.input_backlog_full" => "core.input-backlog-full",
        "room.input_ended" => "core.input-ended",
        "room.message_conflict" => "core.message-conflict",
        "room.pull_ack_invalid" => "core.pull-ack-invalid",
        "room.pull_binding_invalid" => "core.pull-binding-invalid",
        "room.pull_binding_superseded" => "core.pull-binding-superseded",
        "room.pull_cursor_invalid" => "core.pull-cursor-invalid",
        "room.pull_operation_invalid" => "core.pull-operation-invalid",
        "room.receipt_invalid" => "core.receipt-invalid",
        "room.replay_full" => "core.replay-full",
        "room.replay_missing" => "core.replay-missing",
        "room.replay_unavailable" => "core.replay-unavailable",
        "room.request_invalid" => "core.request-invalid",
        "room.speech_eligibility_invalid" => "core.speech-eligibility-invalid",
        "room.speech_invalid" => "core.speech-invalid",
        "room.speech_conflict" => "core.speech-conflict",
        "room.speech_context_invalid" => "core.speech-context-invalid",
        "room.stale_utterance" => "core.stale-utterance",
        "room.state_save_failed" => "core.state-save-failed",
        "room.text_empty" => "core.text-empty",
        "room.text_too_long" => "core.text-too-long",
        "room.thread_invalid" => "core.thread-invalid",
        "room.turns_full" => "core.turns-full",
        "room.user_speaking" => "core.user-speaking",
        _ => "core.refused",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [CoreError; 13] = [
        CoreError::LogFilterInvalid,
        CoreError::StartFailed,
        CoreError::AlreadyRunning,
        CoreError::PortInUse,
        CoreError::UnsafeDirectory,
        CoreError::IdentityUnreadable,
        CoreError::Crashed,
        CoreError::RateLimited,
        CoreError::ConnectionLimit,
        CoreError::PairingRefused,
        CoreError::DeviceUnknown,
        CoreError::StateSaveFailed,
        CoreError::BindingUnknown,
    ];

    fn grammar(code: &str) -> bool {
        code.strip_prefix("core.").is_some_and(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !name.starts_with('-')
                && !name.ends_with('-')
        })
    }

    #[test]
    fn codes_are_unique_and_well_formed() {
        let mut seen = std::collections::HashSet::new();
        for error in ALL {
            assert!(grammar(error.code()), "{}", error.code());
            assert!(seen.insert(error.code()), "duplicate {}", error.code());
        }
        assert_eq!(CoreError::AlreadyRunning.code(), "core.already-running");
    }

    #[test]
    fn every_refusal_key_has_its_own_code() {
        let keys: Vec<&str> = include_str!("error.rs")
            .lines()
            .filter_map(|line| line.trim().strip_prefix("\"room."))
            .filter_map(|rest| rest.split('"').next())
            .collect();
        assert!(keys.len() > 25);
        let mut seen = std::collections::HashSet::new();
        for key in keys {
            let wire = format!("room.{key}");
            let code = refusal_code(&wire);
            assert!(grammar(code) && code != "core.refused", "{wire}");
            assert!(seen.insert(code), "duplicate {code}");
        }
        assert_eq!(refusal_code("room.made_up_by_a_peer"), "core.refused");
        assert_eq!(
            CoreError::Refused("room.focus_changed").code(),
            "core.focus-changed"
        );
    }

    #[test]
    fn a_start_key_maps_to_its_error() {
        assert_eq!(
            CoreError::from_start_key("bind.core-running"),
            CoreError::AlreadyRunning
        );
        assert_eq!(
            CoreError::from_start_key("whatever"),
            CoreError::StartFailed
        );
    }
}
