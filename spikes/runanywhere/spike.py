"""SPIKE (spike/runanywhere, never merged): one synthesis + one transcription through RunAnywhere's Python binding,
built from source (v0.20.37, build.sh). Models: the Whisper tiny and Piper es_ES davefx directories the desktop
spike installed with our catalogue (sidevoice-desktop spike/runanywhere, examples/runanywhere_spike.rs)."""
import os, sys, time, wave
import numpy as np
import runanywhere as ra
from runanywhere import AudioInput, SttOptions, TtsOptions

store = os.environ.get("SPIKE_MODELS", "/tmp/ra-spike/store/models/runanywhere")
whisper = f"{store}/whisper-tiny/sherpa-onnx-whisper-tiny"
piper = f"{store}/piper-es-es-davefx-medium/vits-piper-es_ES-davefx-medium"
WAV = os.path.join(os.environ.get("W", "/tmp/ra-spike"), "tts.wav")

t = time.time(); ra.initialize(); print(f"initialize: {time.time()-t:.2f}s")
t = time.time()
audio = ra.tts.synthesize("Hola, esto es una prueba de voz de Sidevoice.", TtsOptions(model=piper))
print(f"synthesize: {time.time()-t:.2f}s ->", type(audio).__name__, {k: getattr(audio, k) for k in dir(audio) if not k.startswith('_') and k not in ('samples','audio','data','pcm') and not callable(getattr(audio,k))})
samples = audio.samples()
for name in ():
    if hasattr(audio, name):
        samples = getattr(audio, name); break
print("samples:", samples.dtype, samples.shape, "peak", float(np.abs(samples).max()))
sr = getattr(audio, "sample_rate", 22050)
arr = np.asarray(samples)
if arr.dtype != np.int16:
    arr = (np.clip(arr.astype(np.float32), -1, 1) * 32767).astype(np.int16)
with wave.open(WAV, "wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(sr); w.writeframes(arr.tobytes())
for lang in (None, "es"):
    t = time.time()
    try:
        opts = SttOptions(model=whisper, language=lang) if lang else SttOptions(model=whisper)
        out = ra.stt.transcribe(AudioInput.file(WAV), opts)
        print(f"transcribe (language={lang}): {time.time()-t:.2f}s -> {out.text!r}")
    except Exception as e:
        print(f"transcribe (language={lang}) failed: {type(e).__name__}: {e}")
