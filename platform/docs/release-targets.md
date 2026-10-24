# Shared release targets

`sidevoice_platform::target` owns the release-platform facts for the core, connector runtime/tooling and desktop
installer. It adds no dependency: `std::env::consts` already supplies the binary's build OS, CPU and executable
suffix; a library cannot supply Sidevoice's published-platform policy. This is a data-only concern, so it needs
neither a trait nor OS backend files.

| Release name | Rust release triple | npm OS / CPU | npm platform package | Executable suffix |
|---|---|---|---|---|
| `linux-aarch64` | `aarch64-unknown-linux-gnu` | `linux` / `arm64` | `@sidevoice/sidevoice-linux-arm64` | empty |
| `linux-x86_64` | `x86_64-unknown-linux-gnu` | `linux` / `x64` | `@sidevoice/sidevoice-linux-x64` | empty |
| `macos-aarch64` | `aarch64-apple-darwin` | `darwin` / `arm64` | `@sidevoice/sidevoice-darwin-arm64` | empty |
| `windows-x86_64` | `x86_64-pc-windows-msvc` | `win32` / `x64` | `@sidevoice/sidevoice-win32-x64` | `.exe` |

`TARGETS` lists exactly the connector's currently released targets, in its existing order; all four entries had
`released: true`. `build_target()` selects by the binary's compiled OS/CPU, not the native machine architecture:
an x64 binary under emulation on ARM64 Windows selects `windows-x86_64`, the published `win32-x64` package.
`for_platform()` accepts Rust's OS/CPU names; `by_name()` / `by_triple()` require exact release names/triples. Intel macOS, Windows ARM64, 32-bit CPUs, other OSes
and other ABIs have no entry. The triple is the canonical release build triple, not an ABI probe of the caller;
glibc-floor suffixes such as `.2.28` remain build-tool inputs. `exe_name()` also works for a build OS with no
published target, preserving the connector runtime's existing behavior.

For a known target, its `exe_name(stem)` names programs for that target even when the tool runs on another OS.
Its `npm_package()` projects the npm name. Its `archive_name(stem, label)` preserves both local
`sidevoice-connector-windows-x86_64.tar.zst` and published
`sidevoice-core-0.10.1-windows-x86_64.tar.zst` / `sidevoice-connector-nightly-windows-x86_64.tar.zst` names.
Product stems, version validation, archive root directories, entrypoint locations, the Windows background launcher's
membership and the desktop's Tauri bundle/updater formats remain their consumers' policy. In particular, Tauri's
`darwin-aarch64` updater key is different from the release name `macos-aarch64` and npm's `darwin-arm64`.

## Migration inventory

Core xtask now consumes the shared table for its manifest membership, build selection/triples, archive validation,
executable names and archive names (including compatibility downloads). Its former table, OS/CPU and Linux triple
mappings, suffix helpers and archive formatting are deleted. CI runs the manifest regression test on every native
target. The release matrices and external consumers are unchanged; external consumers must pin a core release
containing this module. Replace the following copies with imports/projections from the shared table,
and delete their redundant facts/helpers rather than adding another table. The anchors record the audited revisions.

Connector: [208e2c9](https://github.com/sidevoice/sidevoice-connector/tree/208e2c951e0b21d87cd0936fcbf556411884cb9f).

| Copy to remove or replace | Shared replacement |
|---|---|
| [`connector/src/platform/target.rs:10`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/connector/src/platform/target.rs#L10) — `Target`/`Host`, host mapping and executable suffix | Re-export shared `target`; `build_target().map(\|t\| t.name)` and build `exe_name()` |
| [`connector/src/core_package.rs:156`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/connector/src/core_package.rs#L156) — bundled core archive naming | `archive_name("sidevoice-core", Some(version))`, retaining the `core/` directory |
| [`xtask/src/main.rs:57`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/xtask/src/main.rs#L57) — `Target`, `TARGETS`, name iterators; [`:136`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/xtask/src/main.rs#L136) — suffix/entrypoint formatting | `TARGETS`, `by_name()`, target `exe_name()` |
| [`xtask/src/util.rs:119`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/xtask/src/util.rs#L119) — host lookup | `build_target()` |
| [`xtask/src/dist.rs:26`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/xtask/src/dist.rs#L26) — Rust host triple discovery; [`:135`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/xtask/src/dist.rs#L135) — local archive name | Build target `triple` and `archive_name(..., None)`; retain the Linux glibc floor |
| [`xtask/src/npm.rs:50`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/xtask/src/npm.rs#L50) — package prefix; [`:57`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/xtask/src/npm.rs#L57) — npm projection helpers | `npm_os`, `npm_cpu`, `npm_package()`; generic npm pack naming stays local |
| [`xtask/src/core.rs:24`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/xtask/src/core.rs#L24) — core executable formatting; [`:103`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/xtask/src/core.rs#L103) — core archive name | Target `exe_name()` / `archive_name()` |
| [`xtask/src/manifest.rs:16`](https://github.com/sidevoice/sidevoice-connector/blob/208e2c951e0b21d87cd0936fcbf556411884cb9f/xtask/src/manifest.rs#L16) — published archive name | `archive_name("sidevoice-connector", Some(label))` |

Desktop: [7564b41](https://github.com/sidevoice/sidevoice-desktop/tree/7564b41284ee26934030588a90a0734008f7ccfa).

| Copy to remove or replace | Shared replacement |
|---|---|
| [`src-tauri/local-host/src/npm.rs:24`](https://github.com/sidevoice/sidevoice-desktop/blob/7564b41284ee26934030588a90a0734008f7ccfa/src-tauri/local-host/src/npm.rs#L24) — platform prefix; [`:43`](https://github.com/sidevoice/sidevoice-desktop/blob/7564b41284ee26934030588a90a0734008f7ccfa/src-tauri/local-host/src/npm.rs#L43) / [`:58`](https://github.com/sidevoice/sidevoice-desktop/blob/7564b41284ee26934030588a90a0734008f7ccfa/src-tauri/local-host/src/npm.rs#L58) — OS/CPU and host mapping | `for_platform()` / `build_target()`, then `npm_package()` |
| [`src-tauri/local-host/src/install.rs:42`](https://github.com/sidevoice/sidevoice-desktop/blob/7564b41284ee26934030588a90a0734008f7ccfa/src-tauri/local-host/src/install.rs#L42) — Unix-only installer name | Build target `exe_name("bin/sidevoice-connector")` |
| [`xtask/src/macos.rs:13`](https://github.com/sidevoice/sidevoice-desktop/blob/7564b41284ee26934030588a90a0734008f7ccfa/xtask/src/macos.rs#L13) — Apple Silicon Rust triple | `by_name("macos-aarch64").unwrap().triple`; keep the Mac-only build selection |

Desktop's old npm mapper recognizes Intel macOS's npm namespace despite there being no published target, and rejects
Windows entirely. The shared table describes the actual releases: Intel macOS returns `None`, Windows x64 maps to
`win32-x64`. Migration must retain the no-bundle refusal and exact-version `optionalDependencies` check; a known
platform does not guarantee a package exists at an older version. This PR does not enable desktop local hosting.
The formats at desktop `xtask/src/publish.rs:87` are Tauri-specific and must not be replaced with core/connector
archive names.

Core tooling migration is complete in this PR. The remaining deletion inventory above is external consumers only.
