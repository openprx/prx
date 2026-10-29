"""Exercise the required job's actual script across change-scope fast paths."""
import re
import itertools
import subprocess
import unittest
from pathlib import Path


class RequiredContractGateTests(unittest.TestCase):
    def test_contract_failure_and_skip_block_all_paths(self):
        workflow = (Path(__file__).resolve().parents[2] / '.github/workflows/ci-run.yml').read_text()
        job = workflow.split('    ci-required:', 1)[1]
        needs = job.split('runs-on:', 1)[0]
        self.assertRegex(needs, r'needs: \[[^\]]*\bcontracts\b')
        script = job.split('              run: |\n', 1)[1]
        script = '\n'.join(line[18:] for line in script.splitlines() if line.startswith('                  '))
        for docs_only, rust_changed in [('true', 'false'), ('false', 'false'), ('false', 'true')]:
            for contracts, windows in itertools.product(('failure', 'skipped', 'cancelled', 'success'), repeat=2):
                expressions = {
                    'github.event_name': 'pull_request',
                    'needs.changes.outputs.docs_only': docs_only,
                    'needs.changes.outputs.rust_changed': rust_changed,
                    'needs.changes.outputs.docs_changed': 'false',
                    'needs.changes.outputs.workflow_changed': 'false',
                    'needs.contracts.result': contracts,
                    'needs.windows-installer.result': windows,
                }
                def substitute(match):
                    return expressions.get(match.group(1).strip(), 'success')
                rendered = re.sub(r'\$\{\{\s*(.*?)\s*\}\}', substitute, script)
                with self.subTest(docs=docs_only, rust=rust_changed, contracts=contracts):
                    result = subprocess.run(['bash', '-c', rendered], capture_output=True, text=True)
                    self.assertEqual(result.returncode == 0, contracts == 'success' and windows == 'success', result.stdout + result.stderr)


if __name__ == '__main__':
    unittest.main()
