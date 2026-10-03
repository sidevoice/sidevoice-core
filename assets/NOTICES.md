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

The selected Rustvani feature fixes `ort` and `ort-sys` at `2.0.0-rc.10` in
`Cargo.lock`. That `ort-sys` release fixes ONNX Runtime at **1.22.0** and verifies
its downloaded native archive against its own pinned SHA-256 table before linking.
The three intended target archive hashes from that pinned table are:

| Target | SHA-256 of native archive |
|---|---|
| Linux x86_64 | `ed1716de95974bf47ab0223ca33734a0b5a5d09a181225d0e8ed62d070aea893` |
| Linux aarch64 | `24e4760207136fc50b854bb5012ab81de6189039cf6d4fd3f5b8d3db7e929f1e` |
| macOS aarch64 | `00fbfd6f08bac2a4e28c66723af900d58d1b4b1c73efba6290637cd3019883d5` |

Source: [`ort-sys` distribution table](https://github.com/pykeio/ort/blob/v2.0.0-rc.10/ort-sys/dist.txt)
and [ONNX Runtime 1.22.0 MIT license](https://github.com/microsoft/onnxruntime/blob/v1.22.0/LICENSE).
The Linux x86_64 static link and model execution are proven by T0 CI; the other
target hashes are recorded for later T7 packaging and are not T0 build claims.

Before a distributable T7 bundle, carry the full Rustvani, Pipecat and Silero notices with
the shipped assets and verify the upstream terms for the converted SmartTurn weights.
