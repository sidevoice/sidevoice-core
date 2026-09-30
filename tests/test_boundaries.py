"""The split is a rule, not a folder layout: the pipeline never imports the control plane, and
neither half imports a web framework.

Two checks, because each misses what the other sees. Importing every module in a fresh interpreter
and reading `sys.modules` catches what an import drags in transitively (Pipecat included); reading
every `import` statement in the source catches the lazy import inside a function that a plain
import never executes.
"""
import ast
import json
import pkgutil
import subprocess
import sys
import unittest
from pathlib import Path

import sidevoice_core

PACKAGE = Path(sidevoice_core.__file__).parent
WEB_FRAMEWORKS = ('fastapi', 'starlette', 'socketio', 'engineio', 'uvicorn', 'aiohttp.web', 'flask', 'django')


def modules(half):
    package = f'sidevoice_core.{half}'
    return [package] + [f'{package}.{info.name}' for info in pkgutil.iter_modules([str(PACKAGE / half)])]


def loaded_after_importing(module):
    """Everything in `sys.modules` once `module` alone has been imported, in an interpreter of its own."""
    code = f'import sys, json, {module}; print(json.dumps(sorted(sys.modules)))'
    result = subprocess.run([sys.executable, '-c', code], capture_output=True, text=True, timeout=120)
    if result.returncode:
        raise AssertionError(f'{module} does not import on its own:\n{result.stderr[-2000:]}')
    return set(json.loads(result.stdout.strip().splitlines()[-1]))


def imports_in(path):
    """Every module name an `import` statement in this file names, resolved against the package."""
    tree = ast.parse(path.read_text(encoding='utf8'))
    relative_to = 'sidevoice_core.' + '.'.join(path.relative_to(PACKAGE).with_suffix('').parts[:-1])
    names = []
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            names += [alias.name for alias in node.names]
        elif isinstance(node, ast.ImportFrom):
            if node.level:
                base = relative_to.split('.')[:len(relative_to.split('.')) - node.level + 1]
                prefix = '.'.join(base + ([node.module] if node.module else []))
            else:
                prefix = node.module or ''
            names += [prefix] + [f'{prefix}.{alias.name}' for alias in node.names]
    return names


def forbidden(names, prefixes):
    return sorted({name for name in names for prefix in prefixes if name == prefix or name.startswith(prefix + '.')})


class ImportDirectionTest(unittest.TestCase):
    maxDiff = None

    def test_the_pipeline_never_imports_the_control_plane_nor_a_web_framework(self):
        for module in modules('pipeline'):
            with self.subTest(module=module):
                loaded = loaded_after_importing(module)
                self.assertEqual(forbidden(loaded, ('sidevoice_core.control', 'sidevoice_core.server')), [],
                                 'the pipeline is what scales on its own; it cannot need the node around it')
                self.assertEqual(forbidden(loaded, WEB_FRAMEWORKS), [])

    def test_the_control_plane_imports_no_web_framework(self):
        for module in modules('control'):
            with self.subTest(module=module):
                loaded = loaded_after_importing(module)
                self.assertEqual(forbidden(loaded, ('sidevoice_core.server',)), [])
                self.assertEqual(forbidden(loaded, WEB_FRAMEWORKS), [])

    def test_no_import_statement_crosses_the_line_even_inside_a_function(self):
        rules = {'pipeline': ('sidevoice_core.control', 'sidevoice_core.server') + WEB_FRAMEWORKS,
                 'control': ('sidevoice_core.server',) + WEB_FRAMEWORKS}
        for half, prefixes in rules.items():
            for path in sorted((PACKAGE / half).glob('*.py')):
                with self.subTest(file=str(path.relative_to(PACKAGE))):
                    self.assertEqual(forbidden(imports_in(path), prefixes), [])

    def test_the_runtime_module_both_halves_read_imports_neither(self):
        path = PACKAGE / 'runtime.py'
        self.assertEqual(forbidden(imports_in(path), ('sidevoice_core',) + WEB_FRAMEWORKS), [])


class CallPortTest(unittest.TestCase):
    """The pipeline needs from a call what CallPort names, and nothing else: an object with exactly those
    attributes and methods serves the turn flow's messages without an AttributeError."""

    def test_an_object_with_exactly_the_port_serves_the_turn_flow(self):
        import typing
        from types import SimpleNamespace
        from sidevoice_core.pipeline.call import CallPort, VoiceCall
        from sidevoice_core.pipeline.settings import LanguageSettings, MicSettings
        names = set(typing.get_type_hints(CallPort)) | {name for name in vars(CallPort) if not name.startswith('_')}
        told = []
        recorder = SimpleNamespace(audio_event=lambda *a, **k: told.append(a), turn_context=lambda *a, **k: told.append(a),
                                   input=lambda *a, **k: None, turn_finished=lambda *a, **k: None)
        call = SimpleNamespace(**{name: None for name in names})
        call.id, call.telemetry, call.latency = 'call-1', recorder, recorder
        call.report_audio_health = lambda health: told.append(health)
        call.report_client_error = lambda data: told.append(data)
        transcriber = SimpleNamespace(on_message=None)
        voice = VoiceCall(call, transcriber, told.append, settings=LanguageSettings(), mic=MicSettings(),
                          choice={'provider': 'browser', 'model': 'm', 'reason': 'explicit'})
        voice.browser_message({'type': 'voice-audio-health', 'data': {'session_id': 'call-1', 'reason': 'stall', 'health': {}}})
        voice.browser_message({'type': 'voice-turn-trace', 'data': {'session_id': 'call-1', 'thread_id': 't', 'revision': 1}})
        voice.browser_message({'type': 'voice-client-error', 'data': {'session_id': 'call-1', 'message': 'x'}})
        self.assertEqual(len(told), 4)
        self.assertEqual(set(vars(call)) - names, set(), 'nothing outside the port was written or needed')


if __name__ == '__main__':
    unittest.main()
