`hola-sala-16k.wav` — "Hola, esto es una prueba de voz de la sala.", synthesized with espeak-ng
(the WASM build already in `node_modules/espeak-ng`, voice `es`, 150 wpm) and resampled to 16 kHz
mono. Silero accepts it as speech (2.6 s of speech, peak probability 0.997), which is all the end-to-end test needs:
a real voice detector, a real turn, no microphone. SHA-256:
`a68664544e41df96bc15d2ce5194797e53ab47be223a841639ea16064030eff0`; first committed with the Core at
`4d6df599239602954a3c6ab503c642eeeab5ca12`.

It is the voice in the Rust integration tests (`rust/tests/`) and the detector self-test `cargo xtask dist` runs
on every packaged archive.
