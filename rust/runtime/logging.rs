//! The process's one subscriber (`docs/logging.md`): human `key=value` lines into `core.log`, and onto stderr only on a
//! terminal (under the connector stderr is `core.stderr.log`, and every line would be written twice).

use std::io::{self, IsTerminal};

use tracing::level_filters::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

use super::event_log::{LogFile, IN_PANIC_HOOK};
use super::Config;

const VARIABLE: &str = "SIDEVOICE_LOG";
/// Where a bare level applies: our own crates and the fixed targets. Every other target stays at `warn` unless the
/// variable names it, so a dependency's debug lines never reach a file because somebody asked for `debug`.
const OURS: [&str; 3] = ["sidevoice_core", "sidevoice_platform", "panic"];

/// Install the subscriber and the panic hook. An error is a log that cannot be opened.
pub(super) fn init(config: &Config) -> io::Result<()> {
    let file = LogFile::open(config.log_file.clone(), dirs::home_dir().as_deref())?;
    let (filter, rejected) = filter(std::env::var(VARIABLE).ok().as_deref());
    let terminal = io::stderr()
        .is_terminal()
        .then(|| fmt::layer().with_writer(io::stderr));
    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_ansi(false)
                .log_internal_errors(false)
                .with_writer(file)
                .and_then(terminal)
                .with_filter(filter),
        )
        .try_init()
        .map_err(io::Error::other)?;
    if rejected {
        tracing::warn!(variable = VARIABLE, "invalid log filter, using the default");
    }
    panic_hook();
    Ok(())
}

/// The filter `SIDEVOICE_LOG` asks for, default `info`; whether the value was refused (and the default used).
fn filter(value: Option<&str>) -> (EnvFilter, bool) {
    match EnvFilter::try_new(rewrite(value.unwrap_or("info"))) {
        Ok(filter) => (filter, false),
        Err(_) => (EnvFilter::new(rewrite("info")), true),
    }
}

/// A target-less directive `<lvl>` becomes `warn,<ours>=<lvl>`; directives that name a target are kept.
fn rewrite(spec: &str) -> String {
    spec.split(',')
        .map(str::trim)
        .filter(|directive| !directive.is_empty())
        .flat_map(|directive| match directive.parse::<LevelFilter>() {
            Ok(_) => std::iter::once("warn".to_owned())
                .chain(OURS.map(|target| format!("{target}={directive}")))
                .collect(),
            Err(_) => vec![directive.to_owned()],
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// A panic is an event through the subscriber, so the writer's home backstop applies. The default hook is not
/// chained: it prints the message raw on a stderr that the connector keeps as `core.stderr.log`.
fn panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        IN_PANIC_HOOK.set(true);
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string payload");
        let location = info.location().map(ToString::to_string).unwrap_or_default();
        let thread = std::thread::current();
        tracing::error!(
            target: "panic",
            location,
            thread = thread.name().unwrap_or("unnamed"),
            panic = message,
            "panic"
        );
        IN_PANIC_HOOK.set(false);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_level_applies_to_our_targets_only() {
        assert_eq!(
            rewrite("debug"),
            "warn,sidevoice_core=debug,sidevoice_platform=debug,panic=debug"
        );
        assert_eq!(
            rewrite("info, reqwest=debug,sidevoice_core::server=trace"),
            "warn,sidevoice_core=info,sidevoice_platform=info,panic=info,reqwest=debug,sidevoice_core::server=trace"
        );
    }

    #[test]
    fn an_unusable_value_falls_back_to_the_default() {
        assert!(!filter(None).1);
        assert!(!filter(Some("debug,hyper=off")).1);
        assert!(filter(Some("info,x=notalevel")).1);
    }
}
