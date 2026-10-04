# Local schema patch

This directory vendors the published `elevenlabs-sdk` 0.1.0 crate from crates.io. Its upstream source is the `v0.1.0` tag at `4051ccf757a4d250db4bf6c89df9c5b07ee5a3c2` in <https://github.com/longcipher/elevenlabs-sdk-rs>; the crates.io archive checksum recorded before patching is `6f102dfd0f25d19ff8777c9629f53e35ab2c605aad4e93d81cc4e81085f0080a`. The crate declares Apache-2.0; `LICENSE-APACHE` carries that license text.

The local delta is limited to `src/types/common.rs` and `src/types/voices.rs`: optional catalogue response fields default when absent, `Voice` retains top-level `language` and legacy `voice_type`, and `VerifiedVoiceLanguage.model_id` defaults when omitted. `Voice.category` is optional so the adapter can use the legacy `voice_type` fallback. SDK service methods, request construction, authentication, HTTP client, streaming, and error transport are unchanged.

When updating the SDK, compare the published model and voice schemas, reapply only these compatibility changes if still required, and retain the sparse catalogue and language-precedence fixture in the parent crate. Do not add another HTTP path to work around the private SDK deserialization helpers. This is a source-maintenance obligation for T4's pinned provider adapter, not a general SDK fork or framework.
