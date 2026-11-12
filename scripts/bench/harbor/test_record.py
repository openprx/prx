import copy
import json
from pathlib import Path
import tempfile
import unittest

from record import build_agent, classify, finalize, scan_secrets, unavailable, write, per_attempt_models, serving_provider, agent_metadata
from profile import render, check, PRICES
import sqlite3


class RecordTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.logs = self.root / 'agent'
        self.logs.mkdir()
        task = self.root / 'task'
        task.mkdir()
        (task / 'task.toml').write_text('[agent]\ntimeout_sec=900\n[environment]\ncpus=1\nmemory="4G"\nstorage="10G"\ndocker_image="prx-fixture:fixed"\n')
        self.secret = 'prx-test-key-never-export'
        self.config = {'job_id': 'run-fixture', 'task': {'path': str(task), 'git_commit_id': 'revision'},
                       'agent': {'env': {'PRX_BENCH_API_KEY': '${PRX_BENCH_API_KEY}'}}}
        self.result = {'id': 'trial-fixture', 'task_name': 'prx-fixture', 'started_at': '2026-11-12T00:00:00Z',
                       'verifier_result': {'rewards': {'reward': 1.0}}, 'exception_info': None}
        self.row = {'usage': {'model': 'prx/model', 'input_tokens': 20, 'output_tokens': 5, 'cost_usd': 0.1},
                    'settlement_provider': 'prx', 'cache_read_input_tokens': 3, 'cache_creation_input_tokens': 2}
        self.metadata = {'git_sha': 'sha', 'binary_sha256': 'binary', 'profile_sha256': 'profile',
                         'instruction_sha256': 'instruction', 'version': 'prx 0.8', 'model_id': 'model',
                         'provider': 'prx', 'settlement_provider': 'prx', 'sampling': {'temperature': 0},
                         'profile_values': {'reliability': {'provider_retries': 1}}, 'agent_timeout_sec': 900,
                         'per_attempt_models': unavailable('no per-attempt ledger at sha')}
        (self.logs / 'prx-exit-code').write_text('0')
        (self.logs / 'prx-stdout.txt').write_text('done')
        (self.logs / 'prx-stderr.txt').write_text('')
        (self.logs / 'costs.jsonl').write_text(json.dumps(self.row) + '\n')
        write(self.logs / 'prx-cost.json', {'unsettled_dispatched_requests': 0})
        write(self.root / 'config.json', self.config)
        write(self.root / 'result.json', self.result)
        (self.root / 'trial.log').write_text('fixture')
        verifier = self.root / 'verifier'
        verifier.mkdir()
        (verifier / 'reward.txt').write_text('1.0')
        (verifier / 'test-stdout.txt').write_text('test passed')
        write(verifier / 'ctrf.json', {'results': {'tests': [{'status': 'passed'}]}})
        self.agent = build_agent(self.logs, self.metadata)

    def test_finalize_all_handbook_fields_and_usage(self):
        record = finalize(self.root)
        for field in ('run_id', 'trial_id', 'created_at', 'benchmark', 'dataset_tag', 'task_id', 'repo_revision',
                      'image_digest', 'harness_name', 'version', 'profile_sha256', 'instruction_sha256',
                      'model_id', 'provider', 'sampling', 'resource_limits', 'score', 'verifier_details',
                      'attempt_status', 'usage', 'cost', 'timings', 'trajectory', 'diff', 'artifacts'):
            self.assertIn(field, record, f'handbook field missing: {field}')
        self.assertEqual(record['attempt_status'], 'pass')
        self.assertEqual(record['dataset_tag'], 'terminal-bench@2.0')
        self.assertEqual(record['task_id'], 'task')
        self.assertEqual(record['usage']['uncached_input'], 15)
        self.assertEqual(record['usage']['cache_write'], 2)
        self.assertEqual(record['cost'], 0.1)
        self.assertEqual(record['resource_limits']['memory_mb'], 4096)
        self.assertEqual(record['resource_limits']['storage_mb'], 10240)
        for value in (record['usage']['reasoning'], record['timings']['queue'], record['diff'],
                      record['resource_limits']['gpu'], record['trajectory']['commands'],
                      record['harness_prompt_sha256'], record['image_digest']):
            self.assertEqual(value['status'], 'unavailable')
            self.assertTrue(value['reason'])
        self.assertNotIn('agent', record, 'config.agent.env must never be copied')
        self.assertEqual(self.agent['costs_rows'], [self.row], 'copied ledger must remain verbatim')
        self.assertNotIn(': null', json.dumps(record))

    def test_all_status_branches(self):
        cases = [(124, None, '', '', 1, 'timeout', 'agent_timeout'),
                 (0, 'AgentTimeoutError', '', '', 1, 'timeout', 'agent_timeout'),
                 (0, 'OtherException', '', '', 1, 'infra_error', 'harbor_exception'),
                 (2, None, 'Reason: missing price', '', 1, 'infra_error', 'config_refusal'),
                 (2, None, 'Reason: budget exhausted', '', 1, 'infra_error', 'config_refusal'),
                 (2, None, 'policy', '', 1, 'refusal', 'harness_policy'),
                 *[(1, None, '', 'Error: All providers/models failed. Available providers: [anthropic]. Unavailable providers: none. Attempts:\nprovider=anthropic model=m attempt 1/2: retryable; error=anthropic API error (' + status + '): overloaded', 1, 'infra_error', 'provider_transport')
                   for status in ('503 Service Unavailable', '429 Too Many Requests', '529 <unknown status code>')],
                 (1, None, '', 'Error: tool failed at http://127.0.0.1:5000/v1', 1, 'fail', 'agent_error'),
                 (1, None, '', 'error sending request for url (http://127.0.0.1/v1/chat/completions)', 0, 'infra_error', 'provider_transport'),
                 (1, None, '', 'dns error', 1, 'infra_error', 'provider_transport'),
                 (1, None, '', 'agent failure', 1, 'fail', 'agent_error'),
                 (0, None, '', '', 0, 'fail', 'verifier_reward'),
                 (0, None, '', '', 1, 'pass', 'verifier_reward'),
                 (137, None, '', '', 1, 'infra_error', 'missing_or_unknown_exit')]
        for code, exc, stdout, stderr, reward, status, reason in cases:
            with self.subTest(code=code, exc=exc, stderr=stderr, stdout=stdout):
                agent = {**self.agent, 'exit_code': code}
                result = {'exception_info': {'exception_type': exc} if exc else None}
                self.assertEqual(classify(agent, result, reward, stdout, stderr), (status, reason))
                write(self.logs / 'prx-agent-side.json', agent)
                write(self.root / 'result.json', {**self.result, **result,
                      'verifier_result': {'rewards': {'reward': reward}}})
                (self.logs / 'prx-stdout.txt').write_text(stdout)
                (self.logs / 'prx-stderr.txt').write_text(stderr)
                finalized = finalize(self.root)
                self.assertEqual((finalized['attempt_status'], finalized['reason']), (status, reason))

    def test_model_and_provider_substitution(self):
        for field in ('model', 'provider'):
            agent = copy.deepcopy(self.agent)
            if field == 'model':
                agent['costs_rows'][0]['usage']['model'] = 'prx/other'
            else:
                agent['costs_rows'][0]['settlement_provider'] = 'other'
            self.assertEqual(classify(agent, {}, 1, '', ''), ('infra_error', 'model_substitution'))

    def test_timeout_without_settlement_is_not_substitution(self):
        (self.logs / 'prx-exit-code').write_text('124')
        (self.logs / 'costs.jsonl').write_text('')
        build_agent(self.logs, self.metadata)
        record = finalize(self.root)
        self.assertEqual(record['attempt_status'], 'timeout')
        self.assertEqual(record['usage'], unavailable('killed_before_settlement'))
        self.assertEqual(record['unsettled_dispatched_requests'], 0)
        agent = {**self.agent, 'exit_code': 124, 'costs_rows': [],
                 'per_attempt_models': {'status': 'available', 'attempts': [
                     {'provider': 'prx', 'model': 'other', 'status': 'failed'}]}}
        self.assertEqual(classify(agent, {}, 0, '', ''), ('timeout', 'agent_timeout'))

    def test_secrets_full_and_prefix_scan_all_export_surfaces(self):
        finalize(self.root)
        scan_secrets(self.root, self.secret)
        for name in ('agent/prx-trial.json', 'agent/prx-agent-side.json', 'agent/prx-stdout.txt',
                     'agent/prx-stderr.txt', 'config.json', 'result.json', 'trial.log'):
            path = self.root / name
            original = path.read_bytes()
            for token in (self.secret, self.secret[:8]):
                path.write_bytes(original + token.encode())
                with self.subTest(name=name, token=token), self.assertRaises(ValueError):
                    scan_secrets(self.root, self.secret)
            path.write_bytes(original)

    def test_setup_exception_still_finalizes_one_record(self):
        for name in ('prx-agent-side.json', 'prx-exit-code', 'costs.jsonl', 'prx-stdout.txt', 'prx-stderr.txt'):
            (self.logs / name).unlink()
        write(self.root / 'result.json', {**self.result, 'exception_info': {'exception_type': 'AgentSetupError'}})
        record = finalize(self.root)
        self.assertEqual(record['attempt_status'], 'infra_error')
        self.assertEqual(record['profile_sha256']['status'], 'unavailable')

    def test_sqlite_provider_attempts_read_from_finalized_turn(self):
        database = self.root / 'brain.db'
        with sqlite3.connect(database) as connection:
            connection.execute('CREATE TABLE message_events (event_type TEXT, raw_payload_json TEXT)')
            payload = {'provider_outcome': {'attempts': [
                {'seq': 1, 'provider': 'prx', 'model': 'model', 'status': 'failed'},
                {'seq': 2, 'provider': 'prx', 'model': 'model', 'status': 'success'}]}}
            connection.execute('INSERT INTO message_events VALUES (?, ?)', ('turn.finalized', json.dumps(payload)))
        evidence = per_attempt_models(database, 'sha')
        self.assertEqual(evidence['status'], 'available')
        self.assertEqual(evidence['persisted_models'], ['model'])
        self.assertEqual(evidence['persisted_attempt_count'], 2)
        self.assertEqual(evidence['attempt_count']['status'], 'unavailable')
        self.assertEqual([a['model'] for a in evidence['attempts']], ['model', 'model'])
        self.assertEqual(per_attempt_models(self.root / 'absent.db', 'sha'), unavailable('no per-attempt ledger at sha'))

    def test_failed_attempt_substitution_cannot_hide_behind_final_model(self):
        agent = {**self.agent, 'per_attempt_models': {'status': 'available', 'attempts': [
            {'seq': 1, 'provider': 'prx', 'model': 'other', 'status': 'failed'},
            {'seq': 2, 'provider': 'prx', 'model': 'model', 'status': 'success'}]}}
        self.assertEqual(classify(agent, {}, 1, '', ''), ('infra_error', 'model_substitution'))

    def test_incomplete_phase_times_use_unavailable_markers(self):
        write(self.root / 'result.json', {**self.result,
              'agent_execution': {'started_at': self.result['started_at'], 'finished_at': None}})
        record = finalize(self.root)
        self.assertEqual(record['timings']['agent_execution']['finished_at']['status'], 'unavailable')

    def test_partial_agent_metadata_finalizes_infra_error(self):
        write(self.logs / 'prx-agent-side.json', {'exit_code': unavailable('missing'), 'costs_rows': []})
        self.assertEqual(finalize(self.root)['attempt_status'], 'infra_error')

    def test_missing_exit_with_harbor_timeout_marks_killed_usage(self):
        (self.logs / 'prx-exit-code').unlink()
        (self.logs / 'costs.jsonl').write_text('')
        build_agent(self.logs, self.metadata)
        write(self.root / 'result.json', {**self.result, 'exception_info': {'exception_type': 'AgentTimeoutError'}})
        self.assertEqual(finalize(self.root)['usage'], unavailable('killed_before_settlement'))

    def test_malformed_collection_keeps_unavailable(self):
        (self.logs / 'prx-exit-code').write_text('partial')
        for ledger in ('{bad json', json.dumps({**self.row, 'cache_read_input_tokens': 100})):
            with self.subTest(ledger=ledger):
                (self.logs / 'costs.jsonl').write_text(ledger)
                agent = build_agent(self.logs, self.metadata)
                self.assertEqual(agent['usage'], unavailable('ledger unreadable'))
                self.assertEqual(finalize(self.root)['attempt_status'], 'infra_error')

    def test_custom_provider_labels_preserve_case_and_port(self):
        self.assertEqual(serving_provider('custom:http://u:p@Host:80/v1'), 'custom:http://[redacted]@Host:80/v1')
        self.assertEqual(serving_provider('custom:http://Host:80/v1'), 'custom:http://Host:80/v1')

    def test_real_metadata_producer_removes_credentials(self):
        provider = f'custom:http://u:{self.secret}@Host:80/v1'
        prices = dict.fromkeys(PRICES, 0)
        path = self.root / 'config.toml'
        profile_text = render(path, 'prx/model', self.secret, prices, provider)
        values = check(path)
        metadata = agent_metadata(values, provider, 'model', prices, 0, 'sha', 'binary',
                                  profile_text, 'instruction', 'prx test', 900)
        self.assertEqual(metadata['profile_values']['cost']['daily_limit_usd'], values['cost']['daily_limit_usd'])
        build_agent(self.logs, metadata)
        scan_secrets(self.root, self.secret)
