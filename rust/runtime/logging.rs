//! The process's one subscriber (`docs/logging.md`): human `key=value` lines into `core.log`, and onto stderr only on a
//! terminal (under the connector stderr is `core.stderr.log`, and every line would be written twice).

use std::backtrace::Backtrace;
use std::io::{self, IsTerminal};

use tracing::level_filters::LevelFilter;
use tracing::Metadata;
use tracing_subscriber::filter::combinator::And;
use tracing_subscriber::filter::{filter_fn, FilterExt, FilterFn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

use super::event_log::{home_spellings, LogFile, IN_PANIC_HOOK, PANIC_LINE_LOST};
use super::Config;
use crate::error::CoreError;

const VARIABLE: &str = "SIDEVOICE_LOG";
/// Where a bare level applies: our own crates and the fixed targets. Every other target stays at `warn` unless the
/// variable names it, so a dependency's debug lines never reach a file because somebody asked for `debug`.
const OURS: [&str; 4] = [
    "sidevoice_core",
    "sidevoice_platform",
    "panic",
    "crash_backtrace",
];
/// The panic's backtrace goes to the file only: a stderr sink (a terminal here, a captured file under the connector)
/// never gets it.
const BACKTRACE_TARGET: &str = "crash_backtrace";
/// What stands in for a panic message that is not a literal.
const OMITTED: &str = "<formatted panic message omitted>";

/// Install the subscriber and the panic hook. An error is a log that cannot be opened.
pub(super) fn init(config: &Config) -> io::Result<()> {
    let file = LogFile::open(config.log_file.clone(), dirs::home_dir().as_deref())?;
    crate::crash::install(
        &config.data_dir,
        config.log_file.clone(),
        home_spellings(dirs::home_dir().as_deref()),
        os_type(std::env::consts::OS),
        host_arch(std::env::consts::ARCH),
    );
    let (directives, rejected) = directives(std::env::var(VARIABLE).ok().as_deref());
    let terminal = io::stderr().is_terminal().then(|| {
        fmt::layer()
            .log_internal_errors(false)
            .with_writer(io::stderr)
            .with_filter(terminal_filter(&directives))
    });
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_ansi(false)
                .log_internal_errors(false)
                .with_writer(file)
                .with_filter(EnvFilter::new(&directives)),
        )
        .with(terminal)
        .try_init()
        .map_err(io::Error::other)?;
    if rejected {
        tracing::warn!(
            error.type = CoreError::LogFilterInvalid.code(),
            variable = VARIABLE,
            "the log filter is not valid, so the default level info is used"
        );
    }
    panic_hook();
    Ok(())
}

/// The directives `SIDEVOICE_LOG` asks for, default `info`; whether the value was refused (and the default used).
fn directives(value: Option<&str>) -> (String, bool) {
    let asked = rewrite(value.unwrap_or("info"));
    match EnvFilter::try_new(&asked) {
        Ok(_) => (asked, false),
        Err(_) => (rewrite("info"), true),
    }
}

/// The filter of a stderr sink: the directives with the backtrace target off, and a target check no directive can
/// bypass (a span directive such as `[call]=debug` is evaluated before a target one).
type TargetCheck = FilterFn<fn(&Metadata<'_>) -> bool>;

fn terminal_filter<S>(directives: &str) -> And<EnvFilter, TargetCheck, S> {
    EnvFilter::new(terminal_directives(directives))
        .and(filter_fn(not_backtrace as fn(&Metadata<'_>) -> bool))
}

fn not_backtrace(meta: &Metadata<'_>) -> bool {
    let target = meta.target();
    target != BACKTRACE_TARGET
        && !target
            .strip_prefix(BACKTRACE_TARGET)
            .is_some_and(|rest| rest.starts_with("::"))
}

/// The same directives for a stderr sink, with the backtrace target off (spelled out, never implied).
fn terminal_directives(directives: &str) -> String {
    format!("{directives},{BACKTRACE_TARGET}=off")
}

/// A target-less directive `<lvl>` becomes `<quieter of lvl and warn>,<ours>=<lvl>`; directives that name a target are
/// kept. Without any bare level the default applies to everything else, so naming one target does not silence the rest
/// (panics included).
fn rewrite(spec: &str) -> String {
    let directives: Vec<&str> = spec
        .split(',')
        .map(str::trim)
        .filter(|directive| !directive.is_empty())
        .collect();
    let bare = |directive: &str| directive.parse::<LevelFilter>().ok();
    let mut out: Vec<String> = if directives.iter().any(|d| bare(d).is_some()) {
        Vec::new()
    } else {
        std::iter::once("warn".to_owned())
            .chain(OURS.map(|target| format!("{target}=info")))
            .collect()
    };
    for directive in directives {
        match bare(directive) {
            Some(level) => {
                out.push(level.min(LevelFilter::WARN).to_string());
                out.extend(OURS.map(|target| format!("{target}={directive}")));
            }
            None => out.push(directive.to_owned()),
        }
    }
    out.join(",")
}

/// What a panic leaves behind (`docs/logging.md`, section 9): one `error` event on target `panic` and, for the file
/// only, a second on `crash_backtrace`. Both go through the subscriber, so the writer's home backstop applies. The
/// default hook is never chained: it prints the raw message on a stderr that the connector keeps as `core.stderr.log`.
/// The message is recorded only when it is a literal; a formatted one can hold a path, a name or a secret.
#[allow(
    clippy::disallowed_macros,
    reason = "the panic must stay visible when the log is unwritable"
)]
fn panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        IN_PANIC_HOOK.set(true);
        PANIC_LINE_LOST.set(false);
        let payload = info.payload();
        let literal = payload.downcast_ref::<&'static str>().copied();
        let formatted_len = payload.downcast_ref::<String>().map_or(0, String::len);
        let (file, line) = info
            .location()
            .map_or(("", 0), |location| (location.file(), location.line()));
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        match literal {
            Some(message) => tracing::event!(
                target: "panic",
                tracing::Level::ERROR,
                exception.type = "panic",
                code.file.path = file,
                code.line.number = line,
                thread.name = thread,
                service.version = env!("CARGO_PKG_VERSION"),
                exception.message = message,
                "a thread panicked and stopped; the core may stop with it"
            ),
            None => tracing::event!(
                target: "panic",
                tracing::Level::ERROR,
                exception.type = "panic",
                code.file.path = file,
                code.line.number = line,
                thread.name = thread,
                service.version = env!("CARGO_PKG_VERSION"),
                exception.message = OMITTED,
                len = formatted_len,
                "a thread panicked and stopped; the core may stop with it"
            ),
        }
        let backtrace = Backtrace::force_capture();
        tracing::event!(
            target: "crash_backtrace",
            tracing::Level::ERROR,
            exception.stacktrace = ?backtrace,
            "backtrace of the panic recorded above"
        );
        // After the log events, so the record's tail ends with the panic itself.
        crate::crash::capture(&crate::crash::Panic {
            literal,
            file,
            line,
            backtrace: &backtrace.to_string(),
        });
        let lost = PANIC_LINE_LOST.get();
        IN_PANIC_HOOK.set(false);
        // A log that refused the line would lose the panic. A terminal already shows the event through its own
        // layer; otherwise the location alone (never the message) keeps the panic visible.
        if lost && !io::stderr().is_terminal() {
            eprintln!("sidevoice-core: panic at {file}:{line}");
        }
    }));
}

/// OpenTelemetry's `os.type` for a Rust `std::env::consts::OS` value.
pub(super) fn os_type(os: &str) -> &'static str {
    match os {
        "macos" | "ios" => "darwin",
        "windows" => "windows",
        _ => "linux",
    }
}

/// OpenTelemetry's `os.name`, which tells iOS and Android apart from their kernels.
pub(super) fn os_name(os: &str) -> &'static str {
    match os {
        "macos" => "macOS",
        "ios" => "iOS",
        "windows" => "Windows",
        "android" => "Android",
        _ => "Linux",
    }
}

/// OpenTelemetry's `host.arch` for a Rust `std::env::consts::ARCH` value.
pub(super) fn host_arch(arch: &str) -> &'static str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "x86",
        "arm" => "arm32",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_level_applies_to_our_targets_only() {
        assert_eq!(
            rewrite("debug"),
            "warn,sidevoice_core=debug,sidevoice_platform=debug,panic=debug,crash_backtrace=debug"
        );
        assert_eq!(
            rewrite("info, reqwest=debug,sidevoice_core::server=trace"),
            "warn,sidevoice_core=info,sidevoice_platform=info,panic=info,crash_backtrace=info,reqwest=debug,sidevoice_core::server=trace"
        );
    }

    #[test]
    fn naming_a_target_keeps_the_default_for_the_rest() {
        assert_eq!(
            rewrite("sidevoice_core::server=trace"),
            "warn,sidevoice_core=info,sidevoice_platform=info,panic=info,crash_backtrace=info,sidevoice_core::server=trace"
        );
    }

    #[test]
    fn dependencies_get_the_quieter_of_the_level_and_warn() {
        assert_eq!(
            rewrite("off"),
            "off,sidevoice_core=off,sidevoice_platform=off,panic=off,crash_backtrace=off"
        );
        assert!(rewrite("error").starts_with("error,"));
    }

    #[test]
    fn an_unusable_value_falls_back_to_the_default() {
        assert!(!directives(None).1);
        assert!(!directives(Some("debug,hyper=off")).1);
        let (fallback, rejected) = directives(Some("info,x=notalevel"));
        assert!(rejected);
        assert_eq!(fallback, rewrite("info"));
    }

    /// A sink that keeps what its layer is given.
    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn written(spec: &str, stderr: bool) -> String {
        let sink = Sink::default();
        let kept = sink.clone();
        let (asked, _) = directives(Some(spec));
        let layer = fmt::layer().with_writer(move || sink.clone());
        let subscriber = tracing_subscriber::registry();
        let subscriber = if stderr {
            subscriber.with(layer.with_filter(terminal_filter(&asked)).boxed())
        } else {
            subscriber.with(layer.with_filter(EnvFilter::new(asked)).boxed())
        };
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(target: "panic", "the panic");
            tracing::error!(target: "crash_backtrace", "the backtrace");
        });
        let bytes = kept.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn the_backtrace_target_is_off_on_a_stderr_sink_and_on_in_the_file() {
        for spec in [
            "info",
            "trace",
            "error",
            "panic=trace,crash_backtrace=trace",
        ] {
            let file = written(spec, false);
            assert!(
                file.contains("the panic") && file.contains("the backtrace"),
                "{spec}: {file}"
            );
            let stderr = written(spec, true);
            assert!(stderr.contains("the panic"), "{spec}: {stderr}");
            assert!(!stderr.contains("the backtrace"), "{spec}: {stderr}");
        }
    }

    #[test]
    fn a_span_directive_cannot_bring_the_backtrace_to_the_terminal() {
        for spec in [
            "info,[call]=trace",
            "crash_backtrace[call]=trace",
            "[call]=trace",
        ] {
            let (asked, _) = directives(Some(spec));
            let sink = Sink::default();
            let kept = sink.clone();
            let subscriber = tracing_subscriber::registry().with(
                fmt::layer()
                    .with_writer(move || sink.clone())
                    .with_filter(terminal_filter(&asked)),
            );
            tracing::subscriber::with_default(subscriber, || {
                let _call = tracing::info_span!("call").entered();
                tracing::error!(target: "crash_backtrace", "the backtrace");
                tracing::error!(target: "crash_backtrace::deep", "the nested backtrace");
                tracing::error!(target: "panic", "the panic");
            });
            let out = String::from_utf8(kept.0.lock().unwrap().clone()).unwrap();
            assert!(out.contains("the panic"), "{spec}: {out}");
            assert!(!out.contains("backtrace"), "{spec}: {out}");
        }
    }

    #[test]
    fn the_resource_uses_opentelemetry_values() {
        assert_eq!(os_type("macos"), "darwin");
        assert_eq!(os_type("linux"), "linux");
        assert_eq!(os_type("windows"), "windows");
        assert_eq!(os_name("macos"), "macOS");
        assert_eq!(host_arch("x86_64"), "amd64");
        assert_eq!(host_arch("aarch64"), "arm64");
    }
}
