import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest
from unittest.mock import patch


WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"
DETECTOR = textwrap.dedent(
    WORKFLOW.read_text().split("python3 - <<'PY'\n", 1)[1].split("          PY\n", 1)[0]
)


class CodeChanges(unittest.TestCase):
    def test_paths(self):
        non_code = [
            b"README.md", b"nested/guide.md", b"docs/diagram.svg",
            b"planning/plan.json", b"agents/skills/check.py", b".agents/skills/a/SKILL.md",
            b".claude/settings.json",
            b".github/ISSUE_TEMPLATE/bug.yml", b".github/pull_request_template.md",
            b"skills-lock.json", b"docs/spaces and\nnewlines.txt", b"docs/non-utf8-\xff",
        ]
        code = [
            b"src/main.rs", b"src/provider/prompts/primary.txt", b"tests/test.rs", b"src/x.md", b"tests/fixtures/x.md", b"scripts/notes.md",
            b"crates/new/Cargo.toml", b"Cargo.toml", b"Cargo.lock", b"build.rs",
            b"rust-toolchain.toml", b".cargo/config.toml", b"scripts/check.sh",
            b"fixtures/input.json", b"tests/fixtures/tool-calls.json", b"deny.toml",
            b".github/workflows/ci.yml", b".github/workflows/guide.md",
            b".github/new.yml", b"unknown/file", b"docs-other/input", b"agents-other/file",
            b"planning-other/file", b".agents-other/file", b".claude-other/file",
            b".github/ISSUE_TEMPLATE-other/file", b"LICENSE", b"LICENSE-APACHE",
            b"nested/LICENSE", b"skills-lock.json.bak",
            b"README.MD", b"src/spaces and\nnewlines.rs", b"unknown/non-utf8-\xff",
        ]
        cases = [([path], False) for path in non_code]
        cases += [([path], True) for path in code]
        cases += [(non_code, False), ([], True), ([b"src/old.rs", b"docs/new.md"], True)]
        cases += [([non_code[0], path], True) for path in code]
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            for paths, expected in cases:
                with self.subTest(paths=paths):
                    output.write_text("")
                    diff = b"".join(path + b"\0" for path in paths)
                    with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}), patch(
                        "subprocess.check_output", return_value=diff
                    ) as git:
                        exec(compile(DETECTOR, str(WORKFLOW), "exec"), {})
                    self.assertEqual(output.read_text(), f"code-changed={str(expected).lower()}\n")
                    git.assert_called_once_with([
                        "git", "diff", "--no-renames", "--name-only", "-z", "HEAD^1", "HEAD",
                    ])

    def test_failed_diff_cannot_emit_skip(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            output.touch()
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}), patch(
                "subprocess.check_output", side_effect=subprocess.CalledProcessError(1, "git")
            ), self.assertRaises(subprocess.CalledProcessError):
                exec(compile(DETECTOR, str(WORKFLOW), "exec"), {})
            self.assertEqual(output.read_text(), "")


if __name__ == "__main__":
    unittest.main()
