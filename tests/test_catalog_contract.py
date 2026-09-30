"""The voice catalogue exists twice until one side owns it: here, as the settings validator's package
data, and in rubasace/sidevoice `packages/browser-audio/catalog.json`, which the page's Kokoro reads.
This is the one place that says whether they still agree (it runs when SIDEVOICE_REPOSITORY names a
checkout, and skips otherwise). Which copy should be the only one is an open decision in HANDOVER.md."""
import json
import os
import unittest
from importlib.resources import files
from pathlib import Path


class CatalogContractTest(unittest.TestCase):
    def test_the_core_and_the_page_know_the_same_voices(self):
        page = Path(os.environ.get('SIDEVOICE_REPOSITORY') or '/nonexistent') / 'packages' / 'browser-audio' / 'catalog.json'
        if not page.exists():
            self.skipTest('SIDEVOICE_REPOSITORY does not name a rubasace/sidevoice checkout')
        core = json.loads(files('sidevoice_core.pipeline').joinpath('catalog.json').read_text(encoding='utf8'))
        self.assertEqual(core, json.loads(page.read_text(encoding='utf8')))
