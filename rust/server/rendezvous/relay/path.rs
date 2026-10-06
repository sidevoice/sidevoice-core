//! Which paths the room may reach on this Core.

use percent_encoding::percent_decode_str;
use url::Url;

#[cfg(test)]
mod tests;

const RELAYED: [&str; 4] = [
    "/api/presentation",
    "/api/device",
    "/api/models",
    "/api/host",
];

fn relayed(path: &str, local_only: impl Fn(&str) -> bool) -> bool {
    RELAYED
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
        && !local_only(path)
}

/// Check both every percent-decoded spelling and the path `url::Url` will
/// actually request. The caller supplies T1's local-only rule, so there is one
/// owner for the TCP/UDS exclusion policy.
pub(super) fn relayable(base: &Url, path: &str, local_only: impl Fn(&str) -> bool) -> bool {
    if !path.starts_with('/') || path.starts_with("//") || path.contains('?') || path.contains('#')
    {
        return false;
    }
    let mut current = path.to_owned();
    for _ in 0..=path.len() {
        if !relayed(&current, &local_only) || current.contains("..") || current.contains('\\') {
            return false;
        }
        let next = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if next == current {
            let Ok(final_url) = base.join(path) else {
                return false;
            };
            return final_url.origin() == base.origin() && relayed(final_url.path(), local_only);
        }
        current = next;
    }
    false
}
