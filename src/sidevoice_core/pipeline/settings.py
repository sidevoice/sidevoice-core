"""The settings a device brings to the node, and their defaults. The node keeps none of them.

Every client stores its own configuration and sends it when it connects; the node validates it, uses it
for that call, and forgets it with the call.

Each stage — transcription (`stt`) and voice (`tts`) — is one shape: where it runs (`place`: this device, the
host, or a provider of the model catalogue that does that task), which model, that model's `options`, and
optionally which `build` runs it. Options are checked against the schema the catalogue gives the model's family
(on the device or the host) or the provider's task, never against a list written here: a new model or a new
option is a catalogue change, not a code change. A provider's model ids are the provider's, so any well-formed
one is accepted — it may have shipped a model since Sidevoice did.
"""
import json
import re
from importlib.resources import files
from typing import Any, ClassVar, Literal
from pydantic import BaseModel, ConfigDict, Field, ValidationError, model_validator

from ..models import catalog as model_catalog
from ..models.catalog import PLACES
from ..models.offers import offers

# The voice catalogue is this package's data, its one owner: the browser's build copies it, never the reverse.
# Its languages are the ones a reply can be spoken in.
CATALOG = json.loads(files(__package__).joinpath('catalog.json').read_text(encoding='utf8'))
LANGUAGES = {item['id']: item for item in CATALOG['languages']}

# The model catalogue, read once: what a stage may name and the option schemas it is checked against.
MODELS_CATALOG = model_catalog.load()
FAMILIES = MODELS_CATALOG['families']
MODELS = {item['id']: item for item in MODELS_CATALOG['models']}
PROVIDERS = {item['id']: item for item in MODELS_CATALOG['providers']}

MODEL_ID = re.compile(r'^[A-Za-z0-9][A-Za-z0-9._:-]{0,119}$')
TEXT_LIMIT = 1000        # a text option that names no `max` of its own
VOICE_ID_LIMIT = 120     # a provider's voice id: the provider's to define, ours to bound

# What a stage placed on the host is refused with until the host runs models (sidevoice/sidevoice-core#21). A key the client
# translates, and the sentence in English for one that does not know the key.
HOST_UNAVAILABLE = {'key': 'place_host_unavailable', 'message': 'Running models on the host is not available yet.'}


def catalogue_model(model_id, task):
    """The catalogue's model `model_id` if it does `task`, else None."""
    model = MODELS.get(model_id) if isinstance(model_id, str) else None
    return model if model is not None and FAMILIES[model['family']]['task'] == task else None


def speech_language(tag):
    """A voice's language as a reply's: its primary subtag (`en-us` and `en-gb` both speak `en`)."""
    return tag.split('-', 1)[0]


def _default(option):
    return float(option['default']) if option['kind'] == 'range' else option['default']


def _shown(value):
    """A value a device sent, as a refusal quotes it back: recognisable, and never longer than a line."""
    text = repr(value)
    return text if len(text) <= 40 else text[:39] + '…'


def _voice_id(option, voice, model, language):
    """One voice id: a model voice of that language when the option lists the model's voices, else any id the
    provider may have (bounded)."""
    if option.get('from') == 'model.voices':
        spoken = {entry['id']: speech_language(entry['language']) for entry in (model or {}).get('voices', [])}
        if not isinstance(voice, str) or voice not in spoken:
            raise ValueError(f'{option["id"]}: {_shown(voice)} is not a voice of this model')
        if language is not None and spoken[voice] != language:
            raise ValueError(f'{option["id"]}: {_shown(voice)} does not speak {language}')
        return voice
    if not isinstance(voice, str) or not voice.strip() or len(voice) > VOICE_ID_LIMIT:
        raise ValueError(f'{option["id"]}: a voice id is a non-empty string of at most {VOICE_ID_LIMIT} characters')
    return voice


def _option_value(option, value, model):
    """`value` for `option`, by the option's kind, or ValueError saying why not."""
    name, kind = option['id'], option['kind']
    if kind == 'language':
        if value in option.get('values', []) or (value == 'auto' and option.get('auto')):
            return value
        raise ValueError(f'{name}: {_shown(value)} is not one of its languages')
    if kind == 'text':
        limit = option.get('max', TEXT_LIMIT)
        if not isinstance(value, str) or len(value) > limit:
            raise ValueError(f'{name}: text of at most {limit} characters')
        return value
    if kind == 'range':
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not option['min'] <= value <= option['max']:
            raise ValueError(f'{name}: a number from {option["min"]} to {option["max"]}')
        return float(value)
    if kind == 'voice':
        if not option.get('per_language'):
            return _voice_id(option, value, model, None)
        if not isinstance(value, dict):
            raise ValueError(f'{name}: one voice per speech language, as {{language: voice}}')
        for language in value:
            if language not in LANGUAGES:
                raise ValueError(f'{name}: {_shown(language)} is not a speech language')
        return {language: _voice_id(option, voice, model, language) for language, voice in value.items()}
    raise ValueError(f'{name}: an option of kind {kind!r} cannot be set')


def _options(schema, given, model):
    """The options a stage runs with: each one given, checked by its kind; each one missing, its default (none
    for an option that has no default — a voice is chosen when a reply is spoken). Unknown ids are refused."""
    known = {option['id']: option for option in schema}
    unknown = sorted(set(given) - set(known))
    if unknown:
        raise ValueError('unknown options: ' + ', '.join(_shown(name) for name in unknown[:5]))
    result = {}
    for option in schema:
        if option['id'] in given:
            result[option['id']] = _option_value(option, given[option['id']], model)
        elif 'default' in option:
            result[option['id']] = _default(option)
    return result


class Build(BaseModel):
    """Which engine runs the model, and on what: *Avanzado*'s override of the resolver's choice."""
    model_config = ConfigDict(extra='forbid')
    engine: str = Field(min_length=1, max_length=60)
    accelerator: str = Field(min_length=1, max_length=40)


class Stage(BaseModel):
    """Where one stage runs, with which model and options. `TASK` is what the stage does (`stt`, `tts`)."""
    model_config = ConfigDict(extra='forbid')
    TASK: ClassVar[str]
    place: str = Field(min_length=1, max_length=60)
    model: str = Field(min_length=1, max_length=120)
    options: dict[str, Any] = Field(default_factory=dict)
    build: Build | None = None

    @model_validator(mode='after')
    def fits_the_catalogue(self):
        if not MODEL_ID.fullmatch(self.model):
            raise ValueError(f'{_shown(self.model)} is not a model id')
        if self.place in PLACES:
            model = catalogue_model(self.model, self.TASK)
            if model is None:
                raise ValueError(f'{_shown(self.model)} is not a {self.TASK} model of the catalogue')
            if self.build is not None and self.build.engine not in {build['engine'] for build in model['builds']}:
                raise ValueError(f'{self.model} has no build on {_shown(self.build.engine)}')
            schema = FAMILIES[model['family']].get('options', [])
        else:
            provider = PROVIDERS.get(self.place)
            if provider is None or self.TASK not in provider.get('tasks', []):
                raise ValueError(f'{_shown(self.place)} is not a place that does {self.TASK}')
            if self.build is not None:
                raise ValueError('a provider runs its own models: a build cannot be chosen')
            model, schema = None, provider[self.TASK].get('options', [])
        self.options = _options(schema, self.options, model)
        return self


class Transcription(Stage):
    TASK: ClassVar[str] = 'stt'


class Voice(Stage):
    TASK: ClassVar[str] = 'tts'


STAGES = {'stt': Transcription, 'tts': Voice}


def default_stage(task, capabilities=None, language=None):
    """What a device that chose nothing runs `task` with: its best offer on this device — the resolver's first
    for `capabilities` — or, when those are not known (the node cannot measure a client) or offer nothing, the
    catalogue's first model of the task. Options are the schema's defaults, except that a language option takes
    `language` when it is one of its own (the device's system language), else its default, which is English."""
    chosen = None
    if capabilities is not None:
        chosen = next((offer['model'] for offer in offers(MODELS_CATALOG, capabilities, 'device')
                       if offer['task'] == task), None)
    if chosen is None:
        chosen = next(item['id'] for item in MODELS_CATALOG['models'] if FAMILIES[item['family']]['task'] == task)
    options = {}
    for option in FAMILIES[MODELS[chosen]['family']].get('options', []):
        if option['kind'] == 'language' and language in option.get('values', []):
            options[option['id']] = language
        elif 'default' in option:
            options[option['id']] = _default(option)
    return STAGES[task](place='device', model=chosen, options=options)


class LanguageSettings(BaseModel):
    model_config = ConfigDict(extra='ignore')
    ui_language: Literal['es', 'en'] = 'en'
    stt: Transcription = Field(default_factory=lambda: default_stage('stt'))
    tts: Voice = Field(default_factory=lambda: default_stage('tts'))
    audio_grace_seconds: float = Field(default=1.0, ge=0, le=10)
    # How far back a browser that comes back is played what it never heard (0 is off). This is a
    # preference a person perceives and chooses — how much of the last minutes they want repeated in
    # the car — so it belongs to the device, unlike the detector's tuning, which is the room's.
    replay_on_return_seconds: float = Field(default=120, ge=0, le=3600)
    # How patient the room is with this person's pauses. The only turn-detection choice a device makes:
    # what it means in seconds is the room's, in one place, so a fix reaches everybody (see PATIENCE).
    turn_patience: Literal['fast', 'normal', 'calm'] = 'normal'
    # Microphone defaults for a device that sends none of its own (see MicSettings).
    turn_end_mode: Literal['timer', 'smart_turn'] = 'smart_turn'
    user_speech_timeout: float = Field(default=2.5, ge=0.5, le=15)
    # The floor before smart-turn is even asked. Six tenths cut people mid-sentence when they paused to
    # breathe, even with an intonation that clearly went on (2026-09-20, from a car).
    smart_turn_min_silence: float = Field(default=0.9, ge=0.1, le=3)
    smart_turn_max_silence: float = Field(default=3.0, ge=0.5, le=15)
    vad_confidence: float = Field(default=0.6, ge=0.1, le=1)
    # How loud a sound must be to count as speech at all. The room's own voice, out of a phone's speaker
    # and back into its microphone, arrives well under a person talking into it: at 0.35 it opened turns
    # and cut the reply that was still playing (2026-09-20, the room answering itself word for word).
    vad_min_volume: float = Field(default=0.5, ge=0, le=1)
    # How long the detector must hear voice before it opens a turn (and interrupts a reply). 80 ms opened turns
    # on 96 ms blips while the room's own voice left a car speaker (2026-09-19); the audio before the onset is kept.
    # Half a second held the blips off but made interrupting feel heavy from a moving car, so 0.4 (2026-09-20).
    vad_start_secs: float = Field(default=0.4, ge=0.05, le=1)
    # How long a finished turn waits before delivery, in case the pause was a breath (see MicSettings).
    merge_window_secs: float = Field(default=0.5, ge=0, le=5)


def on_host(settings):
    """Whether either stage is placed on the host, which runs no models yet."""
    return 'host' in (settings.stt.place, settings.tts.place)


def unavailable(settings, config=None):
    """Why a call cannot run on these settings, as the refusal a client is told (a key it translates, the
    provider, an English sentence), or None when it can. Checked for every stage, at the hello and on every
    live change, so a stage that cannot work is refused before it is used instead of failing a reply later:
    the host runs no models yet; a provider needs its key on this node; a provider's voice needs at least one
    voice chosen — its "automatic" is the client's to resolve into one before saving."""
    from . import integrations
    if on_host(settings):
        return dict(HOST_UNAVAILABLE)
    for stage in (settings.stt, settings.tts):
        if stage.place in PLACES:
            continue
        label = PROVIDERS[stage.place].get('label', stage.place)
        if not integrations.key(stage.place, config):
            return {'key': 'provider_key_missing', 'provider': stage.place,
                    'message': f'{label} needs an API key before connecting.'}
        voice = stage.options.get('voice', {}) if stage.TASK == 'tts' else None
        if voice is not None and not (voice.values() if isinstance(voice, dict) else voice):
            return {'key': 'voice_missing', 'provider': stage.place,
                    'message': f'Choose a voice for {label} before connecting.'}
    return None


def load_settings():
    """The node's defaults: what a device that saved nothing gets."""
    return LanguageSettings()


def settings_from(data):
    """A device's settings as it sent them, or the defaults and the reason they were not accepted."""
    if not isinstance(data, dict) or not data:
        return LanguageSettings(), None
    try:
        return LanguageSettings.model_validate(data), None
    except ValidationError as error:
        # One value this node cannot read — a device carrying settings from another version — used to throw
        # every setting away, the transcription provider with them, and an iPhone put back on browser Whisper
        # never finished a turn. Only what was refused falls back to its default; the rest stands. A stage
        # is one setting: anything wrong in it puts the whole stage back to its default, never half of it.
        refused = {item['loc'][0] for item in error.errors() if item.get('loc')}
        reason = 'Some device settings were not valid and use their defaults: ' + '; '.join(
            '.'.join(str(part) for part in item.get('loc', ('?',))) + ' ' + item.get('msg', '') for item in error.errors()[:3])
        try:
            return LanguageSettings.model_validate({k: v for k, v in data.items() if k not in refused}), reason
        except ValidationError:
            return LanguageSettings(), reason


def resolve_voice(settings, language=None):
    """Who says one reply, and how: the stage's place and model, the voice chosen for the reply's language (the
    utterance's, else the interface's), and the speed. On this device a voice must speak that language: the one
    chosen for it if it does, else the model's first that does. A provider's voices are its own and often speak
    several languages, so one chosen for another language is used before none. No voice at all is a ValueError,
    as is a language no voice can speak."""
    language = language or settings.ui_language
    if language not in LANGUAGES:
        raise ValueError('Unsupported speech language.')
    stage = settings.tts
    if stage.place == 'host':
        raise ValueError(HOST_UNAVAILABLE['message'])
    chosen = stage.options.get('voice')
    picked = chosen.get(language) if isinstance(chosen, dict) else chosen
    schema = (FAMILIES[MODELS[stage.model]['family']].get('options', []) if stage.place in PLACES
              else PROVIDERS[stage.place][stage.TASK].get('options', []))
    option = next((entry for entry in schema if entry['kind'] == 'voice'), {})
    if option.get('from') == 'model.voices':
        spoken = [entry['id'] for entry in MODELS[stage.model].get('voices', [])
                  if speech_language(entry['language']) == language]
        voice = picked if picked in spoken else next(iter(spoken), None)
    else:
        voice = picked or (next(iter(chosen.values()), None) if isinstance(chosen, dict) else None)
    if not voice:
        raise ValueError(f'No voice speaks {language} with these settings.')
    speed = next((entry for entry in schema if entry['id'] == 'speed'), None)
    return {'place': stage.place, 'model': stage.model, 'voice': voice, 'language': language,
            'speed': stage.options.get('speed', _default(speed) if speed and 'default' in speed else 1.0)}


class MicSettings(BaseModel):
    """How one device's microphone turns are detected. The room keeps defaults; each browser may send its own."""
    model_config = ConfigDict(extra='ignore')
    turn_end_mode: Literal['timer', 'smart_turn'] = 'smart_turn'
    user_speech_timeout: float = Field(default=2.5, ge=0.5, le=15)
    # Smart-turn is only asked after this much silence: too early and a breath ends the turn.
    # The floor before smart-turn is even asked. Six tenths cut people mid-sentence when they paused to
    # breathe, even with an intonation that clearly went on (2026-09-20, from a car).
    smart_turn_min_silence: float = Field(default=0.9, ge=0.1, le=3)
    smart_turn_max_silence: float = Field(default=3.0, ge=0.5, le=15)
    vad_confidence: float = Field(default=0.6, ge=0.1, le=1)
    # How loud a sound must be to count as speech at all. The room's own voice, out of a phone's speaker
    # and back into its microphone, arrives well under a person talking into it: at 0.35 it opened turns
    # and cut the reply that was still playing (2026-09-20, the room answering itself word for word).
    vad_min_volume: float = Field(default=0.5, ge=0, le=1)
    # How long the detector must hear voice before it opens a turn (and interrupts a reply). 80 ms opened turns
    # on 96 ms blips while the room's own voice left a car speaker (2026-09-19); the audio before the onset is kept.
    # Half a second held the blips off but made interrupting feel heavy from a moving car, so 0.4 (2026-09-20).
    vad_start_secs: float = Field(default=0.4, ge=0.05, le=1)

    # How long a finished turn waits before it is delivered, in case the person was only drawing breath.
    merge_window_secs: float = Field(default=1.2, ge=0, le=5)

    # A device chooses how patient the room is with it, and nothing else about turn detection. Seven numbers
    # nobody can judge by ear (two silences, a timeout, a mode, and the detector's three) were offered before,
    # and a device that had saved the old ones silently kept them, so a fix never reached the person it was
    # written for (2026-09-20). One word does the whole set, coherently, in one place for everyone.
    FIELDS: ClassVar[tuple[str, ...]] = ()
    ROOM_ONLY: ClassVar[tuple[str, ...]] = ('turn_end_mode', 'user_speech_timeout', 'smart_turn_min_silence',
                                            'smart_turn_max_silence', 'vad_confidence', 'vad_min_volume',
                                            'vad_start_secs', 'merge_window_secs')


# What each patience means, as the numbers the pipeline needs. 'normal' is the room's default shape.
PATIENCE = {
    'fast': {'smart_turn_min_silence': 0.6, 'smart_turn_max_silence': 2.5, 'user_speech_timeout': 2.0,
             'merge_window_secs': 0},
    'normal': {'smart_turn_min_silence': 0.9, 'smart_turn_max_silence': 3.0, 'user_speech_timeout': 2.5,
               'merge_window_secs': 0.5},
    'calm': {'smart_turn_min_silence': 1.3, 'smart_turn_max_silence': 4.0, 'user_speech_timeout': 3.5,
             'merge_window_secs': 1.5},
}


def mic_settings(settings, overrides=None):
    """How this call detects turns: the room's numbers, shaped by the one thing the device chooses.

    Returns (settings, problem). The detector's tuning is never read from what a browser sent — not from
    its overrides and not from its stored settings, which is how a device kept the old numbers after the
    room had changed them (2026-09-20). The device's patience is a word; the room turns it into seconds.
    """
    room = load_settings()
    base = {key: getattr(room, key) for key in MicSettings.ROOM_ONLY}
    patience = getattr(settings, 'turn_patience', None)
    if isinstance(overrides, dict) and isinstance(overrides.get('turn_patience'), str):
        patience = overrides['turn_patience']
    if patience not in PATIENCE:
        problem = None if patience is None else 'Unknown patience; the room\'s own is used: ' + str(patience)[:40]
        return MicSettings(**base), problem
    return MicSettings(**{**base, **PATIENCE[patience]}), None
