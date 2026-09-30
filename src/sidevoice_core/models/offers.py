"""The resolver: which models a place can run, each on its best build, with the others ranked behind it.

A pure function of the catalogue and what the place has, and the reference for the two other
implementations (the web's TypeScript for *Este dispositivo*, the desktop's Rust): all three pass
`vectors.json`. Change the rules here, then the vectors, then those two.

Capabilities are what a place reports about itself:

    {"runs": "page", "has": ["webgpu", "webgpu-f16", "wasm"]}
    {"runs": "native", "os": "macos", "arch": "aarch64", "has": ["cpu", "coreml", "metal"], "memory_mb": 16384}

`has` are accelerators and features alike (`webgpu-f16` is a feature a build may need); `memory_mb` is
optional, since pages rarely know it. An offer is

    {"model", "task", "engine", "accelerator", "download_size", "reason",
     "alternatives": [{"engine", "accelerator"}, …]}

where `download_size` is what the first use downloads (the engine's package, if native and not bundled,
plus the build's own files; a page engine fetches its own and reports 0), and `alternatives` are every
other build × accelerator this place can run, best first — what *Avanzado* offers.
"""
from .catalog import PLACES


class UnknownPlace(ValueError):
    pass


def _package(engine, capabilities):
    """The engine's package for this OS/architecture whose requirements the place has, if any."""
    has = set(capabilities.get('has', []))
    for package in engine.get('packages', []):
        if package.get('os') != capabilities.get('os'):
            continue
        if package.get('arch') not in (None, capabilities.get('arch')):
            continue
        if set(package.get('requires', [])) <= has:
            return package
    return None


def _fit(build, engine, capabilities):
    """(accelerators best first, package) when this build runs here; None when it does not."""
    has = set(capabilities.get('has', []))
    runs = capabilities.get('runs')
    if engine.get('runs') != runs:
        return None   # a page engine only in a page, a native one only in a native runtime (D5)
    package = None
    if runs == 'native':
        package = _package(engine, capabilities)
        if package is None:
            return None
        usable = package.get('accelerators', [])
    else:
        usable = engine.get('accelerators', [])
    if not set(build.get('needs', [])) <= has:
        return None
    limited = build.get('accelerators')
    accelerators = [a for a in (limited if limited is not None else usable) if a in usable and a in has]
    return (accelerators, package) if accelerators else None


def _platform(capabilities):
    return f'{capabilities.get("os")}-{capabilities.get("arch")}' if capabilities.get('runs') == 'native' else 'page'


def _size(build, package):
    engine = 0 if package is None or package.get('bundled') else package['download']['size']
    return engine + (build.get('download') or {}).get('size', 0)


def offers(catalog, capabilities, place):
    """One offer per model this place can run, in catalogue order."""
    if place not in PLACES:
        if any(provider['id'] == place for provider in catalog.get('providers', [])):
            return []   # a provider's models are the provider's to list (`models: "remote"`), not ours
        raise UnknownPlace(f'unknown place {place!r}')
    engines = {engine['id']: engine for engine in catalog['engines']}
    order = catalog.get('ranking', {}).get('default', [])
    platform = _platform(capabilities)
    memory = capabilities.get('memory_mb')
    result = []
    for model in catalog['models']:
        needed = model.get('requires', {}).get('memory_mb')
        if needed is not None and memory is not None and memory < needed:
            continue
        fitting = []
        for index, build in enumerate(model['builds']):
            engine = engines.get(build['engine'])
            fit = _fit(build, engine, capabilities) if engine else None
            if fit is not None:
                fitting.append((index, build, *fit))
        if not fitting:
            continue

        def rank(item):
            index, build = item[0], item[1]
            own = build.get('rank', {}).get(platform)
            default = order.index(build['engine']) if build['engine'] in order else len(order)
            return (own is None, own or 0, default, index)
        fitting.sort(key=rank)
        _, best, accelerators, package = fitting[0]
        if len(fitting) == 1:
            why = 'the only build that runs here'
        elif best.get('rank', {}).get(platform) is not None:
            why = f'this model ranks it first on {platform}'
        else:
            why = "first in the catalogue's engine order"
        pairs = [{'engine': build['engine'], 'accelerator': accelerator}
                 for _, build, usable, _ in fitting for accelerator in usable]
        result.append({
            'model': model['id'],
            'task': catalog['families'][model['family']]['task'],
            'engine': best['engine'],
            'accelerator': accelerators[0],
            'download_size': _size(best, package),
            'reason': f'{best["engine"]} ({accelerators[0]}): {why}',
            'alternatives': pairs[1:],
        })
    return result
