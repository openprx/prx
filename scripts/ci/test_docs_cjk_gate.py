"""Contract checks for the tracked-markdown CJK gate."""

import tempfile
import unittest
from pathlib import Path

from docs_cjk_gate import EXPLICIT_EXCLUDES, inspect, should_scan


class DocsCjkGateTests(unittest.TestCase):
    def test_markdown_with_cjk_is_a_violation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "note.md").write_text("hello 中文\n", encoding="utf-8")
            result = inspect(root, ["note.md"])
            self.assertEqual(result["checked"], 1)
            self.assertEqual(len(result["violations"]), 1)
            self.assertEqual(result["violations"][0]["path"], "note.md")
            self.assertEqual(result["violations"][0]["hits"][0]["line"], 1)

    def test_english_markdown_is_clean(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "note.md").write_text("hello\n", encoding="utf-8")
            result = inspect(root, ["note.md"])
            self.assertEqual(result["checked"], 1)
            self.assertEqual(result["violations"], [])

    def test_product_locale_zh_json_is_excluded(self):
        self.assertIn("console/src/lib/i18n/zh.json", EXPLICIT_EXCLUDES)
        self.assertFalse(should_scan("console/src/lib/i18n/zh.json"))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            locale = root / "console" / "src" / "lib" / "i18n"
            locale.mkdir(parents=True)
            (locale / "zh.json").write_text('{"title": "控制台"}\n', encoding="utf-8")
            (root / "note.md").write_text("ok\n", encoding="utf-8")
            result = inspect(
                root,
                ["console/src/lib/i18n/zh.json", "note.md"],
            )
            self.assertEqual(result["checked"], 1)
            self.assertEqual(result["violations"], [])


if __name__ == "__main__":
    unittest.main()
