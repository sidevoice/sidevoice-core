//! Unix: `Sidevoice` in `~/Library/Application Support` on macOS, `sidevoice` in `$XDG_DATA_HOME` on Linux.

/// This OS's data directory.
pub struct Os;

impl super::DataDir for Os {
    const NAME: &'static str = if cfg!(target_os = "macos") {
        "Sidevoice"
    } else {
        "sidevoice"
    };
}
