//! Renderings of the values a log line may carry (`docs/logging.md`, section 8): the rule is in the type, not at the
//! call site.

use std::fmt;

use sha2::{Digest, Sha256};

/// An id a client, agent or page named (`turn`, `session`, `message_id`, `utterance`): as it is when it matches
/// `[A-Za-z0-9._:-]{1,64}`, else `invalid`. The sender chooses the bytes, so an id could otherwise forge a log line.
pub struct ClientId<'a>(pub &'a str);

impl fmt::Display for ClientId<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let id = self.0;
        let plain = |byte: u8| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte);
        if (1..=64).contains(&id.len()) && id.bytes().all(plain) {
            out.write_str(id)
        } else {
            out.write_str("invalid")
        }
    }
}

/// A conversation id we do not mint, as logs name it: `h:` and the first 12 hex digits of SHA-256 over the exact
/// UTF-8 id, the same function in every repository so lines correlate across processes.
pub struct IdHash<'a>(pub &'a str);

impl fmt::Display for IdHash<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("h:")?;
        Sha256::digest(self.0.as_bytes())[..6]
            .iter()
            .try_for_each(|byte| write!(out, "{byte:02x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_id_prints_only_inside_its_grammar() {
        assert_eq!(ClientId("turn-1.a:B_c").to_string(), "turn-1.a:B_c");
        for hostile in [
            "",
            "has space",
            "line\nbreak",
            "canary words 7f3a",
            &"x".repeat(65),
        ] {
            assert_eq!(ClientId(hostile).to_string(), "invalid", "{hostile:?}");
        }
        assert_eq!(ClientId(&"x".repeat(64)).to_string(), "x".repeat(64));
    }

    #[test]
    fn an_id_hash_is_the_shared_one() {
        assert_eq!(
            IdHash("cursor-editor-00000000-0000-0000-0000-000000000000").to_string(),
            "h:64b0598f2d9d"
        );
    }
}
