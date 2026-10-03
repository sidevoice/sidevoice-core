"""One-shot, operator-run provider smoke; never part of routine CI or Core startup."""

import argparse
import http.client
import json
import os
import signal
import time
import wave
from pathlib import Path
from urllib.parse import quote


MAX_AUDIO_SECONDS = 5.0
MAX_TEXT_CHARACTERS = 120
TIMEOUT_SECONDS = 20.0
TOTAL_TIMEOUT_SECONDS = 45.0
MAX_TTS_BYTES = 5_000_000


def preflight(args):
    if not args.permit_paid_once and not args.check_only:
        raise ValueError("Pass --permit-paid-once only for the deliberate manual smoke")
    if not args.wav.is_file() or args.wav.stat().st_size == 0:
        raise ValueError("A nonprivate WAV file is required")
    with wave.open(str(args.wav), "rb") as audio:
        duration = audio.getnframes() / audio.getframerate()
        if (audio.getnchannels(), audio.getsampwidth()) != (1, 2):
            raise ValueError("The WAV must be mono PCM16")
        if not 0 < duration <= MAX_AUDIO_SECONDS:
            raise ValueError("The WAV must contain at most five seconds of audio")
    if not 0 < len(args.tts_text) <= MAX_TEXT_CHARACTERS:
        raise ValueError("The nonprivate TTS text must contain 1 to 120 characters")
    if not args.voice_id or "/" in args.voice_id:
        raise ValueError("A single ElevenLabs voice ID is required")
    openai_key = os.environ.get("VOICE_STT_API_KEY")
    eleven_key = os.environ.get("VOICE_ELEVENLABS_API_KEY")
    if not openai_key or not eleven_key:
        raise ValueError("VOICE_STT_API_KEY and VOICE_ELEVENLABS_API_KEY are required")
    if args.receipt.exists():
        raise ValueError("The one-shot receipt exists; do not rerun this smoke")
    return duration, openai_key, eleven_key


def save_receipt(path, value, exclusive=False):
    flags = os.O_WRONLY | os.O_CREAT | (os.O_EXCL if exclusive else os.O_TRUNC)
    descriptor = os.open(path, flags, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        json.dump(value, output, sort_keys=True, separators=(",", ":"))
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wav", type=Path, required=True)
    parser.add_argument("--tts-text", required=True)
    parser.add_argument("--voice-id", required=True)
    parser.add_argument("--stt-model", default="gpt-4o-mini-transcribe")
    parser.add_argument("--tts-model", default="eleven_v3")
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--permit-paid-once", action="store_true")
    parser.add_argument("--check-only", action="store_true",
                        help="Validate the bounded SDK configuration without sending any request")
    args = parser.parse_args()
    duration, openai_key, eleven_key = preflight(args)

    # This is the pinned Python SDK from uv.lock, separate from Rust Core's
    # production async-openai adapter. Its explicit zero prevents SDK retries.
    from openai import DefaultHttpx2Client, OpenAI

    client = OpenAI(api_key=openai_key, max_retries=0, timeout=TIMEOUT_SECONDS,
                    http_client=DefaultHttpx2Client(
                        timeout=TIMEOUT_SECONDS, follow_redirects=False))
    if client.max_retries != 0:
        raise ValueError("OpenAI SDK retries are not disabled")
    if args.check_only:
        client.close()
        print(json.dumps({"check_only": True, "openai_sdk_max_retries": 0,
                          "redirects": False, "per_request_timeout_seconds": TIMEOUT_SECONDS,
                          "total_timeout_seconds": TOTAL_TIMEOUT_SECONDS}, sort_keys=True))
        return

    receipt = {"kind": "manual-provider-smoke", "stt_model": args.stt_model,
               "tts_model": args.tts_model, "wav_seconds": duration,
               "tts_characters": len(args.tts_text), "stt_attempts": 0,
               "tts_attempts": 0, "sdk_retries": 0, "status": "armed"}
    save_receipt(args.receipt, receipt, exclusive=True)
    def deadline_expired(_signum, _frame):
        raise TimeoutError("Manual smoke reached its total deadline")

    signal.signal(signal.SIGALRM, deadline_expired)
    signal.setitimer(signal.ITIMER_REAL, TOTAL_TIMEOUT_SECONDS)
    try:
        receipt["stt_attempts"] = 1
        receipt["status"] = "stt_started"
        save_receipt(args.receipt, receipt)
        started = time.monotonic()
        with args.wav.open("rb") as audio:
            transcript = client.audio.transcriptions.create(
                file=(args.wav.name, audio, "audio/wav"), model=args.stt_model)
        receipt["stt_ms"] = round((time.monotonic() - started) * 1_000, 1)
        if not transcript.text.strip():
            raise ValueError("STT returned an empty transcript")
        receipt["status"] = "stt_passed"
        save_receipt(args.receipt, receipt)

        # The standard-library client has no retry or redirect policy. This
        # single request call is the entire ElevenLabs budget.
        body = json.dumps({"text": args.tts_text, "model_id": args.tts_model},
                          separators=(",", ":")).encode()
        connection = http.client.HTTPSConnection("api.elevenlabs.io", timeout=TIMEOUT_SECONDS)
        receipt["tts_attempts"] = 1
        receipt["status"] = "tts_started"
        save_receipt(args.receipt, receipt)
        started = time.monotonic()
        try:
            connection.request("POST", "/v1/text-to-speech/" + quote(args.voice_id, safe="")
                               + "?output_format=mp3_44100_128", body=body,
                               headers={"xi-api-key": eleven_key, "content-type": "application/json",
                                        "accept": "audio/mpeg"})
            response = connection.getresponse()
            audio_bytes = response.read(MAX_TTS_BYTES + 1)
            receipt["tts_ms"] = round((time.monotonic() - started) * 1_000, 1)
            if response.status != 200 or not audio_bytes or len(audio_bytes) > MAX_TTS_BYTES:
                raise ValueError(f"TTS failed with HTTP {response.status} or an invalid audio size")
        finally:
            connection.close()
        receipt["tts_audio_bytes"] = len(audio_bytes)
        receipt["status"] = "passed"
    except Exception:
        receipt["status"] = "failed_no_retry"
        raise
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        save_receipt(args.receipt, receipt)
        client.close()
        print(json.dumps(receipt, sort_keys=True))


if __name__ == "__main__":
    main()
