//! Windows: `Sidevoice` in `%LOCALAPPDATA%`.

/// This OS's data directory.
pub struct Os;

impl super::DataDir for Os {
    const NAME: &'static str = "Sidevoice";
}
