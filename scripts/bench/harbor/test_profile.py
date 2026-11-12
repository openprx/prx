import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from profile import PRICES, check, identity, render


class ProfileTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'config.toml'
        self.prices = dict.fromkeys(PRICES, 1.0)
        self.render()

    def render(self):
        return render(self.path, 'prx/prx-fixture', 'prx-fixture-secret', self.prices,
                      'custom:http://127.0.0.1:12345/v1')

    def test_required_profile_values(self):
        c = check(self.path)
        self.assertEqual(c['reliability'], {'fallback_providers': [], 'model_fallbacks': {},
                                         'api_keys': [], 'provider_retries': 1})
        self.assertEqual(c['agent']['compaction'], {'mode': 'off', 'memory_flush': False})
        self.assertEqual(c['memory'], {'backend': 'sqlite', 'auto_save': False, 'embedding_provider': 'none'})
        self.assertFalse(c['auth']['codex_auth_json_auto_import'])
        self.assertEqual(c['autonomy'], {'level': 'full', 'workspace_only': False,
            'forbidden_paths': [], 'acknowledge_unrestricted_profile': True, 'max_cost_per_day_cents': 100000})
        self.assertEqual(c['cost']['daily_limit_usd'], 1000)
        self.assertEqual(c['cost']['monthly_limit_usd'], 10000)
        self.assertEqual(c['tool_tiering']['always_exclude'], ['delegate', 'sessions_spawn'])
        self.assertEqual(c['providers']['models'][0]['operations'], ['chat'])
        self.assertTrue(c['providers']['models'][0]['native_tools'])
        self.assertNotIn('router', c)
        self.assertFalse(c['decision_engine']['enabled'])
        self.assertEqual(self.path.stat().st_mode & 0o777, 0o600)

    def test_all_substitution_surfaces_rejected(self):
        base = self.path.read_text()
        changes = [
            base.replace('fallback_providers = []', 'fallback_providers = ["other"]'),
            base.replace('model_fallbacks = {}', 'model_fallbacks = {"prx-fixture" = ["prx-other-fixture"]}'),
            base.replace('api_keys = []', 'api_keys = ["other-key"]'),
        ]
        changes += [base + f'\n[{table}]\n{key} = [{{ model = "other" }}]\n'
                    for table, key in [('router', 'models'), ('query_classification', 'rules'), ('task_routing', 'rules')]]
        changes += [f'{key} = [{{ model = "other" }}]\n' + base for key in ('model_routes', 'embedding_routes')]
        for changed in changes:
            with self.subTest(profile=changes.index(changed)):
                self.path.write_text(changed)
                with self.assertRaises(ValueError):
                    check(self.path)

    def test_static_fallback_check_cli(self):
        self.path.write_text(self.path.read_text().replace('model_fallbacks = {}',
                            'model_fallbacks = {"prx-fixture" = ["prx-other-fixture"]}'))
        with tempfile.TemporaryFile() as output:
            result = subprocess.run([sys.executable, str(Path(__file__).with_name('profile.py')),
                                     'check', str(self.path)], stdout=output, stderr=output, timeout=30)
            self.assertNotEqual(result.returncode, 0, 'injected fallback must fail static validation')

    def test_removed_enabled_keys_refused_even_false(self):
        base = self.path.read_text()
        for table in ('router', 'query_classification', 'task_routing'):
            for value in ('true', 'false'):
                with self.subTest(table=table, value=value):
                    self.path.write_text(base + f'\n[{table}]\nenabled = {value}\n')
                    with self.assertRaises(ValueError):
                        check(self.path)

    def test_missing_and_invalid_price_refused(self):
        for prices in ({}, {**self.prices, 'input': float('nan')}, {**self.prices, 'output': -1}):
            with self.subTest(prices=prices), self.assertRaises(ValueError):
                render(self.path, 'prx/model', 'secret', prices)

    def test_model_split_and_provider_override(self):
        self.assertEqual(identity('prx/namespace/model'), ('prx', 'namespace/model'))
        with self.assertRaises(ValueError):
            identity('custom:http://localhost/model')
        with self.assertRaises(ValueError):
            identity('prx/model', 'openai')
