//! Fixtures shared by the models tests.

use crate::{models::default_settings, types::CallSettings};

/// The settings a device gets when it reports neither a language nor capabilities.
pub(super) fn defaults() -> CallSettings {
    default_settings(None, None)
}
