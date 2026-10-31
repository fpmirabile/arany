import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("spec-bounds.py")


class SpecBounds(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.spec_dir = self.root / "docs/specs"
        self.spec_dir.mkdir(parents=True)
        rows = (
            ("max_tool_result_bytes", 65536, "bytes"),
            ("max_steering_note_bytes", 8192, "bytes"),
            ("max_read_page_lines", 2000, "lines"),
            ("max_goal_provider_calls", 64, "Provider calls"),
            ("max_goal_runs", 16, "Runs"),
            ("max_window_content_bytes", 1048576, "bytes"),
            ("compaction_input_expansion", 2, "times"),
            ("request_timeout_ms", 500, "ms"),
        )
        table = "| Name | Value | Unit | Owner |\n|---|---:|---|---|\n"
        for anchor, value, unit in rows:
            table += f'| <a id="{anchor}"></a>[`{anchor.upper()}`](#{anchor}) | {value} | {unit} | `owner.rs` |\n'
        (self.spec_dir / "limits.md").write_text(table, encoding="utf-8")
        (self.spec_dir / "tools.md").write_text("A bounded Tool result.\n", encoding="utf-8")

    def run_cli(self, text):
        (self.spec_dir / "tools.md").write_text(text, encoding="utf-8")
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--root", str(self.root)],
            capture_output=True, text=True, check=False, timeout=10,
        )

    def test_cli_rejects_matching_pairs_regardless_of_wording(self):
        for text in (
            "The response is limited to 64 KiB.",
            "Responses may contain at most 64 KiB.",
            "At most 2,000 lines are returned.",
            "The response ceiling is 64 KiB.",
            "The maximum is 64 KiB.",
            "The cap is 64 KiB.",
            "A response must fit in 64 KiB.",
            "A page contains at most 2,000 lines.",
            "Each page returns no more than 2,000 lines.",
            "Tool results are limited to 64 KiB.",
            "The shared Goal defaults retain 16 Runs.",
            "The Git config cap is 64 KiB.",
            "The whole response must fit 1 MiB.",
            "64 KiB.",
            "0.0625 MiB.",
            "The literal is `64 KiB`.",
            "The steering bound is 8-KiB.",
            "The steering bound is 8KiB.",
        ):
            with self.subTest(text=text):
                result = self.run_cli(text + "\n")
                self.assertEqual(result.returncode, 1, result.stdout)
                self.assertIn("docs/specs/tools.md:1: restates generated bound", result.stderr)

    def test_wrapped_units_report_the_numbers_source_line(self):
        for text, mention in (
            ("Introductory text.\nThe ceiling is 64\nKiB.\n", "64 KiB"),
            ("Introductory text.\nThe Goal allows 64 Provider\ncalls.\n", "64 Provider calls"),
        ):
            with self.subTest(text=text):
                result = self.run_cli(text)
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"docs/specs/tools.md:2: restates generated bound {mention}", result.stderr)

    def test_generated_limit_links_are_accepted(self):
        for text in (
            "[64 KiB](./limits.md#max_tool_result_bytes)",
            "[64-KiB](./limits.md#max_tool_result_bytes)",
            "[64KiB](./limits.md#max_tool_result_bytes)",
            "[`64 KiB`](./limits.md#max_tool_result_bytes)",
            "[65536 bytes](limits.md#max_tool_result_bytes)",
            "[2,000 lines](./limits.md#max_read_page_lines)",
            "[Tool result limit](./limits.md#max_tool_result_bytes)",
        ):
            with self.subTest(text=text):
                result = self.run_cli(text)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_domain_folder_files_link_the_parent_limits_table(self):
        folder = self.spec_dir / "tools"
        folder.mkdir()
        for text, code in (
            ("[64 KiB](../limits.md#max_tool_result_bytes)", 0),
            ("[64 KiB](./limits.md#max_tool_result_bytes)", 1),
            ("The response is limited to 64 KiB.", 1),
        ):
            with self.subTest(text=text):
                (folder / "git.md").write_text(text + "\n", encoding="utf-8")
                result = self.run_cli("A bounded Tool result.\n")
                self.assertEqual(result.returncode, code, result.stderr)
                if code:
                    self.assertIn("docs/specs/tools/git.md:1: restates generated bound 64 KiB", result.stderr)
        result = self.run_cli("[64 KiB](../limits.md#max_tool_result_bytes)")
        self.assertEqual(result.returncode, 1)
        self.assertIn("docs/specs/tools.md:1: restates generated bound 64 KiB", result.stderr)

    def test_link_to_different_limit_or_page_does_not_exempt_a_pair(self):
        for text, diagnostic in (
            ("[64 KiB](./limits.md#max_read_page_lines)", "generated limit label 64 KiB does not match max_read_page_lines"),
            ("[64 KiB](./tools.md#limits)", "restates generated bound 64 KiB"),
        ):
            with self.subTest(text=text):
                result = self.run_cli(text)
                self.assertEqual(result.returncode, 1)
                self.assertIn(diagnostic, result.stderr)

    def test_numeric_link_labels_must_equal_target_without_a_global_value_match(self):
        for label in ("32 KiB", "65 KiB", "500 lines", "0.03125 MiB", "65 widgets", "65-KiB", "65KiB"):
            with self.subTest(label=label):
                result = self.run_cli(f"[{label}](./limits.md#max_tool_result_bytes)")
                self.assertEqual(result.returncode, 1)
                mention = "65 KiB" if label in ("65-KiB", "65KiB") else label
                self.assertIn(f"generated limit label {mention} does not match max_tool_result_bytes", result.stderr)

    def test_unknown_generated_anchor_is_rejected(self):
        result = self.run_cli("[Tool result](./limits.md#missing_limit)")
        self.assertEqual(result.returncode, 1)
        self.assertIn("unknown generated limit anchor missing_limit", result.stderr)

    def test_durations_and_multipliers_are_distinct_units(self):
        for text, diagnostic in (
            ("[2 ms](./limits.md#compaction_input_expansion)", "generated limit label 2 ms does not match compaction_input_expansion"),
            ("The expansion is 2 times.", "restates generated bound 2 times"),
            ("The timeout is 0.5 seconds.", "restates generated bound 0.5 seconds"),
        ):
            with self.subTest(text=text):
                result = self.run_cli(text)
                self.assertEqual(result.returncode, 1)
                self.assertIn(diagnostic, result.stderr)
        for text in (
            "The timer expires after 2 ms.",
            "[2 times](./limits.md#compaction_input_expansion)",
            "[0.5 seconds](./limits.md#request_timeout_ms)",
            "The independent expansion is 2 times<!-- unrelated-bound: fixture expansion -->.",
        ):
            with self.subTest(text=text):
                result = self.run_cli(text)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_explicit_marker_accepts_only_its_preceding_pair(self):
        result = self.run_cli("64 KiB<!-- unrelated-bound: Git configuration file cap -->")
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_cli("64 KiB<!-- unrelated-bound: Git configuration file cap --> and 64 KiB")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stderr.count("restates generated bound"), 1)

    def test_marker_requires_an_adjacent_pair_and_nonempty_reason(self):
        for text in (
            "64 KiB<!-- unrelated-bound:  -->",
            "64 KiB<!-- unrelated-bound: -->",
            "64 KiB extra text<!-- unrelated-bound: Git configuration file cap -->",
            "64 KiB\n<!-- unrelated-bound: Git configuration file cap -->",
        ):
            with self.subTest(text=text):
                result = self.run_cli(text)
                self.assertEqual(result.returncode, 1)
                self.assertIn("restates generated bound 64 KiB", result.stderr)

    def test_fenced_samples_and_comment_contents_are_not_prose(self):
        result = self.run_cli(
            "```text\n64 KiB\n```\n<!-- A sample of 64 KiB. -->\n"
            "<!--\n\n64 KiB [sample](./limits.md#missing_limit)\n-->\n"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_cli("````text\n```\n64 KiB\n````\n64 KiB\n")
        self.assertEqual(result.returncode, 1)
        self.assertIn("docs/specs/tools.md:5: restates generated bound", result.stderr)
        self.assertEqual(result.stderr.count("restates generated bound"), 1)

    def test_fences_require_valid_indentation_and_closing_lines(self):
        for opener in ("    ```text", "```text`"):
            with self.subTest(opener=opener):
                result = self.run_cli(f"{opener}\n\n64 KiB\n")
                self.assertEqual(result.returncode, 1)
                self.assertIn("docs/specs/tools.md:3: restates generated bound", result.stderr)
        for marker in ("```", "~~~"):
            with self.subTest(marker=marker):
                result = self.run_cli(f"   {marker}text\n{marker}still-code\n64 KiB\n   {marker} \t\n64 KiB\n")
                self.assertEqual(result.returncode, 1)
                self.assertIn("docs/specs/tools.md:5: restates generated bound", result.stderr)
                self.assertEqual(result.stderr.count("restates generated bound"), 1)


if __name__ == "__main__":
    unittest.main()
