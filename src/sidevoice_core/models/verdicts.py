"""What a model check runs and how its output is judged (sidevoice/sidevoice-core#21, sidevoice/sidevoice-core#13).

Selecting a model loads it and checks it before it takes effect: a transcription model transcribes a bundled
clip of about five seconds and must give back text close to what the clip says; a voice model speaks a fixed
phrase and must give back audio that is not silent and lasts a plausible time. The clips, the phrases and the
thresholds are `checks/checks.json` beside the catalogue, this package's data and their one owner: a node
checks a provider with them (`sidevoice_core.pipeline.model_check`), and the web build copies the directory
to check what a device runs itself — so a client and a host judge alike.

A failed verdict is a refusal like every other the node gives: a stable `key` a client translates, the
parameters its message needs, and an English sentence for one that does not know the key. Pure: no network,
no web framework.
"""
import json
import math
import re
import unicodedata
from functools import cache
from importlib.resources import files

CHECKS = files(__package__).joinpath('checks')


@cache
def spec():
    """`checks.json`: the clips and phrases per language, and the thresholds a check is judged by."""
    return json.loads(CHECKS.joinpath('checks.json').read_text(encoding='utf8'))


def language_for(task, language):
    """The language a check of `task` runs in: `language` when there is a clip or phrase for it, else the
    fallback (English). `auto` and None have none of their own."""
    own = spec()['stt']['clips'] if task == 'stt' else spec()['tts']['phrases']
    return language if language in own else spec()['fallback']


def clip(language):
    """The transcription check for `language` (or the fallback): the WAV's bytes and the words it says."""
    entry = spec()['stt']['clips'][language_for('stt', language)]
    return CHECKS.joinpath(entry['file']).read_bytes(), entry['text']


def phrase(language):
    """The voice check's phrase for `language` (or the fallback)."""
    return spec()['tts']['phrases'][language_for('tts', language)]


def words(text):
    """A text as the words it says: lower case, accents and punctuation gone."""
    plain = unicodedata.normalize('NFKD', str(text or '').lower())
    plain = ''.join(char for char in plain if not unicodedata.combining(char))
    return re.sub(r'[^\w\s]|_', ' ', plain).split()


def word_error(expected, heard):
    """Word error rate of `heard` against `expected`: edits (substitutions, insertions, deletions) over the
    expected words. 0 is word for word; above 1 is possible when far more was heard than said."""
    said, got = words(expected), words(heard)
    if not said:
        return 0.0 if not got else 1.0
    row = list(range(len(got) + 1))
    for i, word in enumerate(said, 1):
        previous, row[0] = row[0], i
        for j, other in enumerate(got, 1):
            previous, row[j] = row[j], min(row[j] + 1, row[j - 1] + 1, previous + (word != other))
    return row[-1] / len(said)


def transcript_problem(expected, heard):
    """Why a transcription check failed, as a refusal, or None when `heard` is close enough to `expected`."""
    if not words(heard):
        return {'key': 'check_silent', 'message': 'The model loaded but produced nothing.'}
    error = word_error(expected, heard)
    if error > spec()['stt']['max_word_error']:
        heard = str(heard).strip()[:200]
        return {'key': 'check_mismatch', 'heard': heard,
                'message': f'The model heard something else: "{heard}".'}
    return None


def audio_problem(samples, rate):
    """Why a voice check failed, as a refusal, or None when the audio is audible and lasts a plausible time.
    `samples` are floats in [-1, 1]."""
    count = len(samples)
    # Audio that is not numbers (NaN, ±inf), or a rate that is not one, is no audio: never "loud enough".
    if not (isinstance(rate, (int, float)) and math.isfinite(rate) and rate > 0) or not all(math.isfinite(value) for value in samples):
        return {'key': 'check_invalid_audio', 'message': 'The model produced audio that is not a valid waveform.'}
    seconds = count / rate
    rms = math.sqrt(sum(value * value for value in samples) / count) if count else 0.0
    if rms < spec()['tts']['min_rms']:
        return {'key': 'check_silent', 'message': 'The model loaded but produced nothing.'}
    low, high = spec()['tts']['seconds']
    if not low <= seconds <= high:
        return {'key': 'check_duration', 'seconds': round(seconds, 2),
                'message': f'The model produced {seconds:.1f} s of audio for a phrase that takes about five.'}
    return None


def slow(latency_ms):
    """Whether a transcription's turn-final latency is above the comfort line: shown, and the person decides
    — never a failure."""
    return latency_ms > spec()['stt']['comfort_ms']
