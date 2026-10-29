"""Contract checks for the tracked-source line limit."""
import tempfile
import unittest
from pathlib import Path

from source_size_gate import inspect


class SourceSizeTests(unittest.TestCase):
    def test_boundaries_and_source_detection(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'nested').mkdir()
            (root / 'nested' / 'good.rs').write_bytes(b'a\r\n' * 4999 + 'é'.encode())
            (root / 'nested' / 'bad.tsx').write_bytes(b'a\n' * 5000 + b'z')
            (root / 'script').write_bytes(b'#!/bin/sh\n' + b'a\n' * 5000)
            (root / 'empty.py').write_bytes(b'')
            result = inspect(root, ['nested/good.rs', 'nested/bad.tsx', 'script', 'empty.py'])
            self.assertEqual(result['checked'], 4)
            self.assertEqual([(x['path'], x['lines']) for x in result['violations']],
                             [('nested/bad.tsx', 5001), ('script', 5001)])

    def test_missing_invalid_and_vendor(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'bad.rs').write_bytes(b'\xff')
            result = inspect(root, ['missing.rs', 'bad.rs', 'vendor/matrix-sdk-0.19.1/generated.rs'])
            self.assertEqual(len(result['violations']), 2)
            self.assertEqual(result['excluded_vendored'], 1)

    def test_escaping_symlink_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'escape.rs').symlink_to('/etc/passwd')
            result = inspect(root, ['escape.rs'])
            self.assertEqual(len(result['violations']), 1)
            self.assertIn('escapes', result['violations'][0]['reason'])


if __name__ == '__main__':
    unittest.main()
