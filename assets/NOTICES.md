# T0 model provenance and notices

The byte sources and SHA-256 digests are in `rust-models.json`. They are pinned to Rustvani
`d01f33e671f7a4d8a128e7bfe55dbf0e8963cb21`; the staging script rejects changed bytes.
The three files are staged outside the Git tree for compilation and detector execution.

- Rustvani code and its SmartTurn conversion are distributed by that project under BSD-2-Clause.
  See its pinned [LICENSE](https://github.com/Allenmylath/rustvani/blob/d01f33e671f7a4d8a128e7bfe55dbf0e8963cb21/LICENSE)
  and [Pipecat notice](https://github.com/Allenmylath/rustvani/blob/d01f33e671f7a4d8a128e7bfe55dbf0e8963cb21/THIRD_PARTY_NOTICES.md).
- Silero VAD's upstream [license](https://github.com/snakers4/silero-vad/blob/master/LICENSE) is MIT.
  The ONNX and converted native Silero files here are the exact bytes committed by Rustvani;
  the native conversion is not used by the T0 runtime probe but Rustvani's build script expects it.
- The committed speech fixture was synthesized locally with espeak-ng, as recorded in
  `tests/fixtures/README.md`; its original Core source is pinned in `tests/fixtures/rust-contract.md`.

Before a distributable T7 bundle, carry the full Rustvani, Pipecat and Silero notices with
the shipped assets and verify the upstream terms for the converted SmartTurn weights.
