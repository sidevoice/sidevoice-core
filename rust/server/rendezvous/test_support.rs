//! Fixtures shared by the rendezvous tests.

use url::Url;

/// The loopback base every test Core listens on.
pub(super) fn loopback() -> Url {
    Url::parse("http://127.0.0.1:8768/").unwrap()
}
