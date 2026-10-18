# Changelog

## [0.4.3](https://github.com/sidevoice/sidevoice-core/compare/v0.4.2...v0.4.3) (2026-10-18)


### Bug Fixes

* **room:** persist explicit conversation leaves across restarts ([#133](https://github.com/sidevoice/sidevoice-core/issues/133)) ([40621b8](https://github.com/sidevoice/sidevoice-core/commit/40621b8cc2c8824c443ce53a0402074d3f653198))

## [0.4.2](https://github.com/sidevoice/sidevoice-core/compare/v0.4.1...v0.4.2) (2026-10-18)


### Bug Fixes

* **server:** advertise private machine addresses in pairing codes ([#131](https://github.com/sidevoice/sidevoice-core/issues/131)) ([5420448](https://github.com/sidevoice/sidevoice-core/commit/54204484d1796abdae2b1105420e9db32f05eb85))

## [0.4.1](https://github.com/sidevoice/sidevoice-core/compare/v0.4.0...v0.4.1) (2026-10-17)


### Bug Fixes

* **server:** accept direct numeric-IP LAN hosts ([#128](https://github.com/sidevoice/sidevoice-core/issues/128)) ([c6bbd79](https://github.com/sidevoice/sidevoice-core/commit/c6bbd79e449f0ff58f882821be77d6bed4386c37))

## [0.4.0](https://github.com/sidevoice/sidevoice-core/compare/v0.3.0...v0.4.0) (2026-10-15)


### Features

* **runtime:** the core's default port is 10829 ([#116](https://github.com/sidevoice/sidevoice-core/issues/116)) ([6e17338](https://github.com/sidevoice/sidevoice-core/commit/6e17338f55f11d0276f03eb3025bf4880e4f0344))
* **server:** forward an agent health check to the connector ([#107](https://github.com/sidevoice/sidevoice-core/issues/107)) ([f7f8d8e](https://github.com/sidevoice/sidevoice-core/commit/f7f8d8e28734d86d9a1160e5be2ef6723c9a534e))
* **server:** rename a paired device (PATCH /api/device/devices/{id}) ([#106](https://github.com/sidevoice/sidevoice-core/issues/106)) ([df591fa](https://github.com/sidevoice/sidevoice-core/commit/df591fa6a5301ca6591a43c423a7548563cf7475))
* **server:** report the machine's Sidevoice version to paired devices (GET /api/host/status) ([#105](https://github.com/sidevoice/sidevoice-core/issues/105)) ([79e36b0](https://github.com/sidevoice/sidevoice-core/commit/79e36b02492a3d7799eb87d6743cda9b52578593))
* **turns:** implement conversation epochs and semantic hearing ([#120](https://github.com/sidevoice/sidevoice-core/issues/120)) ([e81cc67](https://github.com/sidevoice/sidevoice-core/commit/e81cc67602fe6f99bc26f05bfb859ba9b1b6c82c))


### Bug Fixes

* **control:** every wire reader ignores unknown fields and enum values ([#113](https://github.com/sidevoice/sidevoice-core/issues/113)) ([6037be7](https://github.com/sidevoice/sidevoice-core/commit/6037be7469dcf64c43f5710cc1812e9bf8d51d8f))
* **copy:** stop saying room/sala to the user ([#117](https://github.com/sidevoice/sidevoice-core/issues/117)) ([def2bc3](https://github.com/sidevoice/sidevoice-core/commit/def2bc390cec683d27e3a6f69ebfb05b3a83e95b))
* **i18n:** the core speaks English only ([#123](https://github.com/sidevoice/sidevoice-core/issues/123)) ([bebfec5](https://github.com/sidevoice/sidevoice-core/commit/bebfec52f81d18381f84b43df078ea223482fb26))

## [0.3.0](https://github.com/sidevoice/sidevoice-core/compare/v0.2.1...v0.3.0) (2026-10-10)


### ⚠ BREAKING CHANGES

* keep only the room in core: turns and replies as text, no audio ([#91](https://github.com/sidevoice/sidevoice-core/issues/91))

### Features

* keep only the room in core: turns and replies as text, no audio ([#91](https://github.com/sidevoice/sidevoice-core/issues/91)) ([93a040c](https://github.com/sidevoice/sidevoice-core/commit/93a040c359d6803bd7a4dc22c78d3e8675c13ec3))
* resume dropped calls and take client messages once ([#88](https://github.com/sidevoice/sidevoice-core/issues/88)) ([e057270](https://github.com/sidevoice/sidevoice-core/commit/e057270a87d06a6ba3b8613a81d40adb69f06b98))
* **room:** tell the agent what the person did not hear, never play it late ([#90](https://github.com/sidevoice/sidevoice-core/issues/90)) ([a277de4](https://github.com/sidevoice/sidevoice-core/commit/a277de48379c2e9d35de35c704fc173dbc13d65c))


### Bug Fixes

* **compat:** check the core against the released Rust connector ([#92](https://github.com/sidevoice/sidevoice-core/issues/92)) ([4a4e311](https://github.com/sidevoice/sidevoice-core/commit/4a4e3114fd7e05aaafa6cc72c60f9acf4cf7ff4c))
* night sweep, room turn admission, unheard notes, replays, no v2 link or SQLite import ([#96](https://github.com/sidevoice/sidevoice-core/issues/96)) ([b158d53](https://github.com/sidevoice/sidevoice-core/commit/b158d5313e4a4849d8c6a368bda0cd4cdf85269f))

## [0.2.1](https://github.com/sidevoice/sidevoice-core/compare/v0.2.0...v0.2.1) (2026-10-06)


### Miscellaneous Chores

* release 0.2.1 with the Linux build for glibc 2.28 ([#83](https://github.com/sidevoice/sidevoice-core/issues/83)) ([3003f66](https://github.com/sidevoice/sidevoice-core/commit/3003f66589de9790a259687769475a0167b153e3))

## [0.2.0](https://github.com/sidevoice/sidevoice-core/compare/v0.1.0...v0.2.0) (2026-10-06)


### Features

* 'native' as a device choice — the client's own engine, outside the page ([08658a8](https://github.com/sidevoice/sidevoice-core/commit/08658a8ac26884135478b0f0f80c7cc6db188611))
* a desktop shell can use this node directly, and pair it with a room ([50ce662](https://github.com/sidevoice/sidevoice-core/commit/50ce6624f2088767ef913484d3f2a8ea4f1f187b))
* add authenticated connector protocol 3 websocket ([#42](https://github.com/sidevoice/sidevoice-core/issues/42)) ([4d6df59](https://github.com/sidevoice/sidevoice-core/commit/4d6df599239602954a3c6ab503c642eeeab5ca12))
* add one-for-one Rust Core MVP and native bundles ([#51](https://github.com/sidevoice/sidevoice-core/issues/51)) ([b41840e](https://github.com/sidevoice/sidevoice-core/commit/b41840e41e3eb81905d285514c7deb35bd8efe57))
* **core:** opt-in pull delivery from the in-memory input journal ([#55](https://github.com/sidevoice/sidevoice-core/issues/55)) ([0a9da38](https://github.com/sidevoice/sidevoice-core/commit/0a9da3879726526b9d24b1ed077d963497fc4ca7))
* **core:** Rust telemetry parity with the Python core ([#68](https://github.com/sidevoice/sidevoice-core/issues/68)) ([88e2f9a](https://github.com/sidevoice/sidevoice-core/commit/88e2f9af45cd692071bf4bbfb40e11eafb40cdb8))
* **core:** the local socket — the app's pairing and the connector link off TCP, launches named, start failures keyed (onboarding R1-a) ([#29](https://github.com/sidevoice/sidevoice-core/issues/29)) ([6cd273a](https://github.com/sidevoice/sidevoice-core/commit/6cd273a8f07a5de930909c2d44b0c1b438f8771c))
* Cursor's conversations — their route, their experimental capabilities, and a chat that cannot take input ([bd79932](https://github.com/sidevoice/sidevoice-core/commit/bd7993283f3d32161b77c0728cf2d51190916dfb))
* device pairing — the node issues the code, requires the token, proves its identity ([fdb9156](https://github.com/sidevoice/sidevoice-core/commit/fdb915665c79035b3b1d26c6f1206056c360dfdb))
* expose host agent management API ([#38](https://github.com/sidevoice/sidevoice-core/issues/38)) ([2710e84](https://github.com/sidevoice/sidevoice-core/commit/2710e8420c8157d21721f3df32adebcbbb346957))
* integrations — one key per provider, the node's, written by the owner and never read back ([8e958d8](https://github.com/sidevoice/sidevoice-core/commit/8e958d8d3bf7b54a18e9c724ac3dc4a4c9c5e5c5))
* integrations — one key per provider, the node's, written by the owner and never read back ([6862adb](https://github.com/sidevoice/sidevoice-core/commit/6862adb05d123b61e01d88eb4d867362e583e1cb))
* **models:** check a model before it takes effect — check clips, provider check, POST /api/models/check ([#25](https://github.com/sidevoice/sidevoice-core/issues/25)) ([f799bbb](https://github.com/sidevoice/sidevoice-core/commit/f799bbbd53ed7c6a1b381cf84f7c38fb8fc66fec))
* **models:** model catalogue v2, validator, reference resolver, GET /api/models/catalog ([#124](https://github.com/sidevoice/sidevoice-core/issues/124) phase 1) ([9be5576](https://github.com/sidevoice/sidevoice-core/commit/9be557682aede05328fa8d07673a96c4d357cfbb))
* **models:** the model catalogue v2, its validator, the reference resolver and GET /api/models/catalog ([14623b7](https://github.com/sidevoice/sidevoice-core/commit/14623b79573f7626c284ed2e060509ab5c2cdeba))
* **settings:** one stage per task — place, model, options, build — dispatched by place ([#124](https://github.com/sidevoice/sidevoice-core/issues/124) phase 2) ([#3](https://github.com/sidevoice/sidevoice-core/issues/3)) ([63f9705](https://github.com/sidevoice/sidevoice-core/commit/63f9705d42d674c7b026df4bdf9c29462c1005fd))
* split the room's server into a pipeline and a control plane ([dee6257](https://github.com/sidevoice/sidevoice-core/commit/dee62577025d84de10f2fa94b9eba866098bb5de))
* the core runs as a node process its connector can supervise ([aff0ae2](https://github.com/sidevoice/sidevoice-core/commit/aff0ae20f668913b17479808378addb6407f3561))
* the microphone over WebRTC, the call socket as the fallback ([3fbc360](https://github.com/sidevoice/sidevoice-core/commit/3fbc3609ff80cbc8f8c3320ab4afdb39563a9a51))
* the node's side of the rendezvous — a link with the room, and the relay it carries ([704e5b6](https://github.com/sidevoice/sidevoice-core/commit/704e5b6659011c476f3b2289772fdf09541d497b))


### Bug Fixes

* a revoked device's open call ends at once; encoded dot segments cannot leave the relayed surface ([fb69407](https://github.com/sidevoice/sidevoice-core/commit/fb694077e85ffe59eddacabd82f872a6bb97b116))
* **core:** restore the imports the server split took from two handlers ([#74](https://github.com/sidevoice/sidevoice-core/issues/74)) ([3b54861](https://github.com/sidevoice/sidevoice-core/commit/3b54861257c6a04cc4d8a9b59110545fb6ba0cd7))
* **core:** Rust call pipeline parity — turn patience, browser heartbeat, idle audio ([#65](https://github.com/sidevoice/sidevoice-core/issues/65)) ([6449e85](https://github.com/sidevoice/sidevoice-core/commit/6449e85378db81edc60fd8ac2240188bdcb1ad67))
* **core:** Rust parity — catalogue assets, server contracts and Spanish messages ([#67](https://github.com/sidevoice/sidevoice-core/issues/67)) ([7fb6da6](https://github.com/sidevoice/sidevoice-core/commit/7fb6da60f0ebe0a569401208f2346312243e7c51))
* **core:** Rust room parity — missed-reply replay, playback timeout, audio grace ([#70](https://github.com/sidevoice/sidevoice-core/issues/70)) ([4ce413e](https://github.com/sidevoice/sidevoice-core/commit/4ce413e189cc0d2d125ffbb18ade7e25f29ba430))
* pre-publication audit — HTTPS-only credentials, explicit cluster trust, minimal discovery ([#6](https://github.com/sidevoice/sidevoice-core/issues/6)) ([92ea496](https://github.com/sidevoice/sidevoice-core/commit/92ea496bdb88cb685c947a8e72489a13676ee158))
* restore Rust Core call and settings route parity ([#54](https://github.com/sidevoice/sidevoice-core/issues/54)) ([b2ae125](https://github.com/sidevoice/sidevoice-core/commit/b2ae125453baa3634b94eefcc49879588e3b6e40))
* **settings:** a device that sends nothing gets English ([c129f4b](https://github.com/sidevoice/sidevoice-core/commit/c129f4b1c1568dd4c24b16ea0474f35493707a7a))
* what the adversarial review confirmed, and a refusal that was only a dropped room ([9265727](https://github.com/sidevoice/sidevoice-core/commit/926572703c9d095c3845a2930315b4bb4ab96ff4))
