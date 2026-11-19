//! Status to `exception.type`: a closed table, numbers and fixed names only.

/// Which Unix numbers its signals: they differ for `SIGBUS` and `SIGSYS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unix {
    Linux,
    Macos,
}

/// The crash signals by name. A kill (`KILL`, `TERM`, `HUP`, `INT`, `QUIT`, `PIPE`) is a person, a logout or a shutdown.
const SIGNALS: [&str; 7] = ["SEGV", "BUS", "ILL", "FPE", "ABRT", "TRAP", "SYS"];

/// The Windows exceptions that are a crash: access violation, illegal instruction, array bounds, float stack check, stack
/// overflow, heap corruption and fast fail (also how Rust's `abort` ends). Any other code is not reported.
const NTSTATUS: [u32; 7] = [
    0xC000_0005,
    0xC000_001D,
    0xC000_0094,
    0xC000_0095,
    0xC000_00FD,
    0xC000_0374,
    0xC000_0409,
];

const OOM: &str = "abort.oom_kill";

fn number_name(number: i32, unix: Unix) -> Option<&'static str> {
    Some(match (number, unix) {
        (4, _) => "ILL",
        (5, _) => "TRAP",
        (6, _) => "ABRT",
        (8, _) => "FPE",
        (11, _) => "SEGV",
        (7, Unix::Linux) | (10, Unix::Macos) => "BUS",
        (31, Unix::Linux) | (12, Unix::Macos) => "SYS",
        _ => return None,
    })
}

/// The type of the crash signal `number` is on `unix`; `None` for any other signal.
pub fn signal_type(number: i32, unix: Unix) -> Option<String> {
    number_name(number, unix).map(|name| format!("abort.signal.SIG{name}"))
}

/// A signal as systemd says it: its name with or without `SIG`, or its (Linux) number.
fn named_signal(text: &str) -> Option<String> {
    let text = text.trim();
    if let Ok(number) = text.parse::<i32>() {
        return signal_type(number, Unix::Linux);
    }
    let name = text.strip_prefix("SIG").unwrap_or(text);
    SIGNALS
        .contains(&name)
        .then(|| format!("abort.signal.SIG{name}"))
}

/// The type of a Windows exit code, when it is one of the allowlisted exceptions.
pub fn ntstatus_type(code: u32) -> Option<String> {
    NTSTATUS
        .contains(&code)
        .then(|| format!("ntstatus.0x{code:08X}"))
}

/// What systemd hands `ExecStopPost=`: `$SERVICE_RESULT` (`oom-kill`, `signal`, `core-dump`, `exit-code`, ...), `$EXIT_CODE`
/// (`exited`, `killed`, `dumped`) and `$EXIT_STATUS` (the exit number, or the signal's name). A crash is an `oom-kill`
/// result, or a main process killed or dumped by a crash signal **and** a result of `signal` or `core-dump`: a `watchdog` or
/// `timeout` result is systemd's own kill (it signals the process itself), not a crash.
pub fn systemd_type(result: &str, exit_code: &str, status: &str) -> Option<String> {
    match result.trim() {
        "oom-kill" => Some(OOM.to_owned()),
        "signal" | "core-dump" => match exit_code.trim() {
            "killed" | "dumped" => named_signal(status),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `kind` is the end a Rust panic produces when the process aborts (`SIGABRT`; on Windows `0xC0000409`, the fast fail
/// `abort` raises). Only for these may a panic record of the same run stand for the exit: any other end is a different event.
pub fn is_panic_abort(kind: &str) -> bool {
    kind == "abort.signal.SIGABRT" || kind == "ntstatus.0xC0000409"
}

/// Whether `text` is one of the types this module returns: nothing else is ever read back from an exit file or reported.
pub fn is_crash_type(text: &str) -> bool {
    if text == OOM {
        return true;
    }
    if let Some(name) = text.strip_prefix("abort.signal.SIG") {
        return SIGNALS.contains(&name);
    }
    text.strip_prefix("ntstatus.0x").is_some_and(|hex| {
        hex.len() == 8
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
            && u32::from_str_radix(hex, 16).is_ok_and(|code| NTSTATUS.contains(&code))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_signals_are_named_by_number_per_unix() {
        let linux = |n| signal_type(n, Unix::Linux);
        let mac = |n| signal_type(n, Unix::Macos);
        assert_eq!(linux(11).as_deref(), Some("abort.signal.SIGSEGV"));
        assert_eq!(linux(7).as_deref(), Some("abort.signal.SIGBUS"));
        assert_eq!(mac(10).as_deref(), Some("abort.signal.SIGBUS"));
        assert_eq!(mac(7), None);
        assert_eq!(linux(6).as_deref(), Some("abort.signal.SIGABRT"));
        assert_eq!(linux(4).as_deref(), Some("abort.signal.SIGILL"));
        assert_eq!(mac(8).as_deref(), Some("abort.signal.SIGFPE"));
        assert_eq!(linux(31).as_deref(), Some("abort.signal.SIGSYS"));
        assert_eq!(mac(12).as_deref(), Some("abort.signal.SIGSYS"));
        // Kill, term, hang-up, interrupt, quit, pipe, and nonsense are never a crash.
        for number in [9, 15, 1, 2, 3, 13, 0, -1, 64] {
            assert_eq!(linux(number), None, "{number}");
            assert_eq!(mac(number), None, "{number}");
        }
    }

    #[test]
    fn windows_codes_are_reported_only_from_the_allowlist() {
        assert_eq!(
            ntstatus_type(0xC000_0005).as_deref(),
            Some("ntstatus.0xC0000005")
        );
        assert_eq!(
            ntstatus_type(0xC000_00FD).as_deref(),
            Some("ntstatus.0xC00000FD")
        );
        assert_eq!(
            ntstatus_type(0xC000_0409).as_deref(),
            Some("ntstatus.0xC0000409")
        );
        // Console close and logoff, a missing DLL, a failed start, a plain exit code, and an abort without a status.
        for code in [
            0xC000_013A,
            0xC000_0142,
            0xC000_0135,
            0,
            1,
            101,
            0x4013_0001,
        ] {
            assert_eq!(ntstatus_type(code), None, "{code:#x}");
        }
    }

    #[test]
    fn systemd_statuses_are_a_crash_only_when_the_main_process_crashed() {
        let case = |result, code, status| systemd_type(result, code, status);
        assert_eq!(
            case("core-dump", "dumped", "SEGV").as_deref(),
            Some("abort.signal.SIGSEGV")
        );
        assert_eq!(
            case("signal", "killed", "ABRT").as_deref(),
            Some("abort.signal.SIGABRT")
        );
        assert_eq!(
            case("signal", "killed", "SIGBUS").as_deref(),
            Some("abort.signal.SIGBUS")
        );
        assert_eq!(
            case("signal", "killed", "11").as_deref(),
            Some("abort.signal.SIGSEGV")
        );
        assert_eq!(
            case("oom-kill", "killed", "KILL").as_deref(),
            Some("abort.oom_kill")
        );
        // Not crashes: a kill, a term (stop, logout), a clean stop, a handled exit, a status that is not ours.
        assert_eq!(case("signal", "killed", "KILL"), None);
        assert_eq!(case("signal", "killed", "TERM"), None);
        assert_eq!(case("success", "exited", "0"), None);
        assert_eq!(case("exit-code", "exited", "1"), None);
        assert_eq!(case("exit-code", "exited", "11"), None);
        assert_eq!(case("signal", "killed", "SEGV; rm -rf ~"), None);
        // systemd's own kills (a watchdog, a stop that timed out) end in a signal too: not a crash.
        for result in ["watchdog", "timeout", "resources", "start-limit-hit", ""] {
            assert_eq!(case(result, "killed", "ABRT"), None, "{result}");
            assert_eq!(case(result, "dumped", "SEGV"), None, "{result}");
        }
        assert_eq!(case("", "", ""), None);
    }

    #[test]
    fn the_closed_list_accepts_only_what_the_tables_return() {
        for text in [
            "abort.signal.SIGSEGV",
            "abort.oom_kill",
            "ntstatus.0xC0000005",
        ] {
            assert!(is_crash_type(text), "{text}");
        }
        for text in [
            "",
            "panic",
            "abort.signal.SIGKILL",
            "abort.signal.SIGSEGV ",
            "ntstatus.0xC000013A",
            "ntstatus.0xc0000005",
            "ntstatus.0xC0000005\n",
            "ntstatus.0xC00000050",
            "/home/ana/x",
        ] {
            assert!(!is_crash_type(text), "{text:?}");
        }
    }
}
