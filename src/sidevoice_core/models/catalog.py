"""The catalogue as package data, and what it must never contain.

Shape (version 2):

- `engines`: what loads a build and runs it. `runs: native` engines come as `packages` per OS/architecture
  (downloaded, each with its SHA-256 and the SHA-256 of every library loaded from it; or `bundled` inside an
  app, where the stores forbid downloading code); `runs: page` engines run inside a web page and list the
  `accelerators` a page may offer them. Accelerators are listed best first.
- `ranking.default`: which engine wins when a model has several builds that fit, unless the build's own
  `rank` for the platform says otherwise.
- `families`: the task (`stt`, `tts`) and the options every model of the family takes, as a schema a client
  renders and the node validates a device's settings against (a `text` option may cap its length with `max`).
- `providers`: external services a host calls with its own key; their models are theirs to list (`remote`),
  and each task names the options it takes, like a family.
- `models`: what a person picks. Each has one build per engine it runs on: the format, what to download (a
  native engine's files), the engine's own configuration, and optionally the `accelerators` it is limited to
  and the capabilities it `needs` beyond them. A page build's configuration says, per accelerator, the
  precision it loads (`dtype`) and the bytes that downloads (`sizes`): what a page asks consent for.
"""
import json
from functools import cache
from importlib.resources import files
from pathlib import Path

VERSION = 2
TASKS = ('stt', 'tts')
RUNS = ('native', 'page')
# Option kinds a client knows how to render. One outside this list is refused here, in our own catalogue.
OPTION_KINDS = ('language', 'text', 'voice', 'range')
# Where a model can run besides a provider: this client itself, or the host it is paired with.
PLACES = ('device', 'host')


@cache
def data():
    """The catalogue data's one copy: `assets/catalog/` at the repository root, which the Rust core compiles in.
    A wheel carries it as the data package `sidevoice_core._catalog`; a source checkout reads it in place."""
    try:
        return files('sidevoice_core._catalog')
    except ModuleNotFoundError:
        return Path(__file__).resolve().parents[3] / 'assets' / 'catalog'


@cache
def catalog_text():
    """The file exactly as shipped: what the endpoint serves and every copy must equal."""
    return data().joinpath('models').joinpath('catalog.json').read_text(encoding='utf8')


def load():
    """A fresh copy of the catalogue, safe to change."""
    return json.loads(catalog_text())


def vectors():
    """The shared resolver vectors: capabilities in, offers out (see `offers.py`)."""
    return json.loads(data().joinpath('models').joinpath('vectors.json').read_text(encoding='utf8'))


def _sha256(value):
    return isinstance(value, str) and len(value) == 64 and all(c in '0123456789abcdefABCDEF' for c in value)


def _download_problem(download):
    """Native downloads are https and carry their SHA-256 and size: nothing unchecked is ever run."""
    if not isinstance(download, dict):
        return 'needs an https download with sha256'
    if not str(download.get('url', '')).startswith('https://') or not _sha256(download.get('sha256')):
        return 'needs an https download with sha256'
    if not isinstance(download.get('size'), int) or download['size'] <= 0:
        return 'download needs its size'
    return None


def _option_problems(where, options):
    problems, seen = [], set()
    for option in options or []:
        name = option.get('id')
        if not name or name in seen:
            problems.append(f'{where}: option ids must be present and unique ({name!r})')
        seen.add(name)
        if option.get('kind') not in OPTION_KINDS:
            problems.append(f'{where}: option {name} has an unknown kind {option.get("kind")!r}')
        if option.get('kind') == 'range':
            low, high, default = option.get('min'), option.get('max'), option.get('default')
            if not (isinstance(low, (int, float)) and isinstance(high, (int, float)) and low < high):
                problems.append(f'{where}: range {name} needs min < max')
            elif default is not None and not low <= default <= high:
                problems.append(f'{where}: range {name} has its default outside it')
        if option.get('kind') == 'text' and 'max' in option:
            limit = option['max']
            if isinstance(limit, bool) or not isinstance(limit, int) or limit <= 0:
                problems.append(f'{where}: text {name} needs a positive whole max')
    return problems


def check(catalog):
    """Every problem the catalogue has, in words; empty when it is sound. Ported from sidevoice-desktop's
    `engines.rs` `check()`, plus the rules of families, options and providers."""
    problems = []
    if catalog.get('version') != VERSION:
        problems.append(f'version must be {VERSION}')
    engines = {}
    for engine in catalog.get('engines', []):
        name = engine.get('id')
        if name in engines:
            problems.append(f'{name}: engine listed twice')
        engines[name] = engine
        if engine.get('runs') not in RUNS:
            problems.append(f'{name}: runs must be one of {", ".join(RUNS)}')
        if engine.get('runs') == 'native':
            if not engine.get('packages'):
                problems.append(f'{name}: native engine without packages')
            for package in engine.get('packages', []):
                where = f'{name} {package.get("os")}/{package.get("arch", "*")}'
                if not package.get('accelerators'):
                    problems.append(f'{where}: a package lists no accelerators')
                if package.get('bundled'):
                    continue
                if not package.get('arch'):
                    problems.append(f'{where}: a downloaded package names its architecture')
                if problem := _download_problem(package.get('download')):
                    problems.append(f'{where}: {problem}')
                for library in package.get('libraries', []):
                    path = str(library.get('path', ''))
                    if not _sha256(library.get('sha256')) or '..' in path or path.startswith('/') or not path:
                        problems.append(f'{where}: library {path} needs a sha256 and a relative path')
        elif engine.get('runs') == 'page' and not engine.get('accelerators'):
            problems.append(f'{name}: a page engine lists no accelerators')
    for name in catalog.get('ranking', {}).get('default', []):
        if name not in engines:
            problems.append(f'ranking: unknown engine {name}')

    families = catalog.get('families', {})
    for name, family in families.items():
        if family.get('task') not in TASKS:
            problems.append(f'{name}: family task must be one of {", ".join(TASKS)}')
        problems += _option_problems(f'family {name}', family.get('options'))

    providers = set()
    for provider in catalog.get('providers', []):
        name = provider.get('id')
        if name in providers or name in PLACES:
            problems.append(f'{name}: provider id must be unique and not a place')
        providers.add(name)
        for task in provider.get('tasks', []):
            if task not in TASKS:
                problems.append(f'{name}: unknown task {task}')
                continue
            entry = provider.get(task)
            if not isinstance(entry, dict) or not (entry.get('models') == 'remote' or isinstance(entry.get('models'), list)):
                problems.append(f'{name}: {task} needs models, a list or "remote"')
                continue
            problems += _option_problems(f'provider {name} {task}', entry.get('options'))

    models = set()
    for model in catalog.get('models', []):
        name = model.get('id')
        if name in models:
            problems.append(f'{name}: model listed twice')
        models.add(name)
        family = families.get(model.get('family'))
        if family is None:
            problems.append(f'{name}: unknown family {model.get("family")}')
            continue
        memory = model.get('requires', {}).get('memory_mb')
        if memory is not None and not (isinstance(memory, int) and memory > 0):
            problems.append(f'{name}: requires.memory_mb must be a positive whole number')
        on = set()
        for build in model.get('builds', []):
            engine_id = build.get('engine')
            engine = engines.get(engine_id)
            if engine is None:
                problems.append(f'{name}: unknown engine {engine_id}')
                continue
            if engine_id in on:
                problems.append(f'{name}: two builds on {engine_id}')
            on.add(engine_id)
            if model['family'] not in engine.get('families', []):
                problems.append(f'{name} on {engine_id}: the engine does not run the {model["family"]} family')
            if build.get('format') not in engine.get('formats', []):
                problems.append(f'{name} on {engine_id}: format {build.get("format")} not read')
            if engine.get('runs') == 'native' and (problem := _download_problem(build.get('download'))):
                problems.append(f'{name} on {engine_id}: native build {problem}')
            usable = set(engine.get('accelerators', [])) | {
                accelerator for package in engine.get('packages', []) for accelerator in package.get('accelerators', [])}
            if not set(build.get('accelerators', [])) <= usable:
                problems.append(f'{name} on {engine_id}: an accelerator the engine never uses')
        if not model.get('builds'):
            problems.append(f'{name}: no builds')
        voices = [voice.get('id') for voice in model.get('voices', [])]
        if any(option.get('from') == 'model.voices' for option in family.get('options', [])) and not voices:
            problems.append(f'{name}: a voice model lists no voices')
        if len(voices) != len(set(voices)):
            problems.append(f'{name}: a voice listed twice')
    return problems
