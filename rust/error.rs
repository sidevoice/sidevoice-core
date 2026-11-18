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
    #[error("the crash report setting or records cannot be read or changed")]
    CrashReportsUnavailable,
    #[error("a crash report could not be sent")]
    CrashReportFailed,
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
            Self::StateSaveFailed => "core.device-registry-save-failed",
            Self::BindingUnknown => "core.binding-unknown",
            Self::CrashReportsUnavailable => "core.crash-reports-unavailable",
            Self::CrashReportFailed => "core.crash-report-failed",
            Self::Refused(key) => refusal_code(key),
        }
    }

    /// Every code this enum can give, the refusals' included (and `core.refused`, what a refusal that is not listed is called):
    /// the closed list of what the core calls an error. Telemetry sends no `error.type` that is not in it.
    pub fn codes() -> impl Iterator<Item = &'static str> {
        FIXED
            .iter()
            .map(CoreError::code)
            .chain(REFUSALS.iter().map(|(_, code)| *code))
            .chain(std::iter::once(REFUSED))
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

/// Every variant that stands for one code of its own (`Refused` stands for the codes of [`REFUSALS`]).
const FIXED: &[CoreError] = &[
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
    CoreError::CrashReportsUnavailable,
    CoreError::CrashReportFailed,
];

/// The wire key of every refusal a room operation makes, and its code. One table: `refusal_code` reads it and
/// `CoreError::codes` lists it.
const REFUSALS: &[(&str, &str)] = &[
    ("room.binding_closed", "core.binding-closed"),
    ("room.binding_foreign", "core.binding-foreign"),
    (
        "room.binding_thread_mismatch",
        "core.binding-thread-mismatch",
    ),
    ("room.browser_absent", "core.browser-absent"),
    ("room.connector_disconnected", "core.connector-disconnected"),
    ("room.focus_changed", "core.focus-changed"),
    ("room.full", "core.room-full"),
    ("room.input_backlog_full", "core.input-backlog-full"),
    ("room.input_ended", "core.input-ended"),
    ("room.message_conflict", "core.message-conflict"),
    ("room.pull_ack_invalid", "core.pull-ack-invalid"),
    ("room.pull_binding_invalid", "core.pull-binding-invalid"),
    (
        "room.pull_binding_superseded",
        "core.pull-binding-superseded",
    ),
    ("room.pull_cursor_invalid", "core.pull-cursor-invalid"),
    ("room.pull_operation_invalid", "core.pull-operation-invalid"),
    ("room.receipt_invalid", "core.receipt-invalid"),
    ("room.replay_full", "core.replay-full"),
    ("room.replay_missing", "core.replay-missing"),
    ("room.replay_unavailable", "core.replay-unavailable"),
    ("room.request_invalid", "core.request-invalid"),
    (
        "room.speech_eligibility_invalid",
        "core.speech-eligibility-invalid",
    ),
    ("room.speech_invalid", "core.speech-invalid"),
    ("room.speech_conflict", "core.speech-conflict"),
    ("room.speech_context_invalid", "core.speech-context-invalid"),
    ("room.stale_utterance", "core.stale-utterance"),
    ("room.state_save_failed", "core.room-state-save-failed"),
    ("room.text_empty", "core.text-empty"),
    ("room.text_too_long", "core.text-too-long"),
    ("room.thread_invalid", "core.thread-invalid"),
    ("room.turns_full", "core.turns-full"),
    ("room.user_speaking", "core.user-speaking"),
];

/// What a refusal whose key is not in [`REFUSALS`] is called.
const REFUSED: &str = "core.refused";

/// The code of a refusal's wire key. A key that is not listed in [`REFUSALS`] is `core.refused`, so a key is never logged as
/// itself (one may come from a peer).
pub fn refusal_code(key: &str) -> &'static str {
    REFUSALS
        .iter()
        .find(|(wire, _)| *wire == key)
        .map_or(REFUSED, |(_, code)| code)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for error in FIXED {
            assert!(grammar(error.code()), "{}", error.code());
            assert!(seen.insert(error.code()), "duplicate {}", error.code());
        }
        assert_eq!(CoreError::AlreadyRunning.code(), "core.already-running");
    }

    #[test]
    fn every_refusal_key_has_its_own_code() {
        assert!(REFUSALS.len() > 25);
        let mut seen: std::collections::HashSet<&str> = FIXED.iter().map(CoreError::code).collect();
        for (wire, code) in REFUSALS {
            assert_eq!(refusal_code(wire), *code, "{wire}");
            assert!(grammar(code) && *code != REFUSED, "{wire}");
            assert!(seen.insert(code), "duplicate {code}");
        }
        assert_eq!(refusal_code("room.made_up_by_a_peer"), "core.refused");
        assert_eq!(
            CoreError::Refused("room.focus_changed").code(),
            "core.focus-changed"
        );
    }

    #[test]
    fn codes_lists_every_code_once_the_refusals_and_their_fallback_included() {
        let codes: Vec<&str> = CoreError::codes().collect();
        let unique: std::collections::HashSet<&str> = codes.iter().copied().collect();
        assert_eq!(codes.len(), unique.len(), "a code listed twice");
        assert_eq!(codes.len(), FIXED.len() + REFUSALS.len() + 1);
        assert!(codes.iter().all(|code| grammar(code)));
        for error in FIXED {
            assert!(codes.contains(&error.code()), "{}", error.code());
        }
        for (wire, _) in REFUSALS {
            assert!(codes.contains(&CoreError::Refused(wire).code()), "{wire}");
        }
        assert!(codes.contains(&CoreError::Refused("room.made_up_by_a_peer").code()));
        assert!(!codes.contains(&"core.not-one-of-ours"));
    }

    /// `FIXED` is written by hand beside the enum, and what telemetry may send is what it lists: a variant that is added and not
    /// listed would be logged and never reported. The enum's own source is read, so adding one fails here until it is listed.
    #[test]
    fn the_list_of_variants_holds_every_variant_of_the_enum() {
        // A checkout on Windows has CRLF line ends.
        let source = include_str!("error.rs").replace("\r\n", "\n");
        let body = source
            .split("pub enum CoreError {")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("the enum is in this file");
        let declared: Vec<(String, bool)> = body
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with(|c: char| c.is_ascii_uppercase()))
            .map(|line| {
                let name: String = line
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                let unit = line[name.len()..].trim_start() == ",";
                (name, unit)
            })
            .collect();
        assert!(declared.len() > 15, "the enum was not read: {declared:?}");
        let listed: Vec<String> = FIXED.iter().map(|error| format!("{error:?}")).collect();
        for (name, unit) in &declared {
            if *unit {
                assert!(listed.contains(name), "{name} is not in FIXED");
            } else {
                assert_eq!(
                    name, "Refused",
                    "{name} carries data: list its codes in `CoreError::codes`"
                );
            }
        }
        for name in &listed {
            assert!(
                declared
                    .iter()
                    .any(|(declared, unit)| declared == name && *unit),
                "{name} is in FIXED and is not a variant"
            );
        }
        assert_eq!(
            listed.len(),
            declared.iter().filter(|(_, unit)| *unit).count(),
            "a variant listed twice"
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
