`hola-sala-16k.wav` — "Hola, esto es una prueba de voz de la sala.", synthesized with espeak-ng
(the WASM build already in `node_modules/espeak-ng`, voice `es`, 150 wpm) and resampled to 16 kHz
mono. Silero accepts it as speech (2.6 s of speech, peak probability 0.997), which is all the end-to-end test needs:
a real voice detector, a real turn, no microphone.
