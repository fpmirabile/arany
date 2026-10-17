import os
import runpy
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

DETECTOR_PATH = Path(__file__).resolve().with_name("ci-changes.py")
DETECTOR = runpy.run_path(str(DETECTOR_PATH))


class CodeChanges(unittest.TestCase):
    def test_paths(self):
        non_code = [
            b"README.md", b"nested/guide.md", b"docs/diagram.svg",
            b"planning/plan.json", b"agents/skills/check.py", b".agents/skills/a/SKILL.md",
            b".claude/settings.json",
            b".github/ISSUE_TEMPLATE/bug.yml", b".github/pull_request_template.md",
            b"skills-lock.json", b".github/dependabot.yml", b"docs/spaces and\nnewlines.txt", b"docs/non-utf8-\xff",
        ]
        code = [
            b"scripts/notes.md",
            b"crates/new/Cargo.toml", b"crates/arany/README.md", b"crates/arany-cli/tests/fixtures/x.md", b"Cargo.toml", b"Cargo.lock", b"build.rs",
            b"rust-toolchain.toml", b".cargo/config.toml", b"scripts/check.sh",
            b"fixtures/input.json", b"deny.toml",
            b".github/workflows/ci.yml", b".github/workflows/guide.md",
            b".github/actions/ci-tests/action.yml", b".github/actions/ci-tests/README.md",
            b".github/new.yml", b"unknown/file", b"docs-other/input", b"agents-other/file",
            b"planning-other/file", b".agents-other/file", b".claude-other/file",
            b".github/ISSUE_TEMPLATE-other/file", b"LICENSE", b"LICENSE-APACHE",
            b"nested/LICENSE", b"skills-lock.json.bak",
            b"README.MD", b"unknown/spaces and\nnewlines.rs", b"unknown/non-utf8-\xff",
        ]
        cases = [([path], False) for path in non_code]
        cases += [([path], True) for path in code]
        cases += [(non_code, False), ([], True), ([b"unknown/old.rs", b"docs/new.md"], True)]
        cases += [([non_code[0], path], True) for path in code]
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            for paths, expected in cases:
                with self.subTest(paths=paths):
                    output.write_text("")
                    diff = b"".join(path + b"\0" for path in paths)
                    with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output), "CI_EVENT": "pull_request"}), patch(
                        "subprocess.check_output", return_value=diff
                    ) as git:
                        DETECTOR["main"]()
                    self.assertEqual(output.read_text(), f"code-changed={str(expected).lower()}\n")
                    git.assert_called_once_with([
                        "git", "diff", "--no-renames", "--name-only", "-z", "HEAD^1", "HEAD",
                    ], timeout=30)

    def test_failed_diff_cannot_emit_skip(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "output"
            output.touch()
            with patch.dict(os.environ, {"GITHUB_OUTPUT": str(output), "CI_EVENT": "pull_request"}), patch(
                "subprocess.check_output", side_effect=subprocess.CalledProcessError(1, "git")
            ), self.assertRaises(subprocess.CalledProcessError):
                DETECTOR["main"]()
            self.assertEqual(output.read_text(), "")

    def test_unknown_or_missing_push_base_runs_tests_without_git(self):
        for event, before in [
            ("workflow_dispatch", ""), ("unknown", ""), ("push", ""),
            ("push", "0" * 40), ("push", "--upload-pack=unexpected"),
        ]:
            with self.subTest(event=event, before=before), patch("subprocess.run") as fetch, patch(
                "subprocess.check_output"
            ) as diff:
                self.assertTrue(DETECTOR["code_changed"](event, before))
                fetch.assert_not_called()
                diff.assert_not_called()

    def test_failed_push_fetch_cannot_emit_skip(self):
        for error in [subprocess.CalledProcessError(1, "git"), subprocess.TimeoutExpired("git", 30)]:
            with self.subTest(error=type(error).__name__), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "output"
                output.touch()
                with patch.dict(os.environ, {
                    "GITHUB_OUTPUT": str(output), "CI_EVENT": "push", "CI_BEFORE": "a" * 40,
                }), patch("subprocess.run", side_effect=error), self.assertRaises(type(error)):
                    DETECTOR["main"]()
                self.assertEqual(output.read_text(), "")


class GitChanges(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.env = {
            "PATH": os.defpath,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "CI fixture",
            "GIT_AUTHOR_EMAIL": "ci@example.invalid",
            "GIT_COMMITTER_NAME": "CI fixture",
            "GIT_COMMITTER_EMAIL": "ci@example.invalid",
        }
        self.git("init", "--template=", "-b", "main")
        self.commit("README.md")
        self.base = self.git("rev-parse", "HEAD").strip()
        self.git("remote", "add", "origin", str(self.repo))

    def git(self, *args):
        return subprocess.check_output(
            ["git", *args], cwd=self.repo, env=self.env, stderr=subprocess.DEVNULL,
            timeout=10, text=True,
        )

    def commit(self, path):
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(path)
        self.git("add", path)
        self.git("commit", "-m", "Add fixture")

    def detect(self, event, expected):
        output = self.root / "output"
        output.write_text("")
        subprocess.run(
            ["python3", str(DETECTOR_PATH)], cwd=self.repo,
            env={**self.env, "CI_EVENT": event, "CI_BEFORE": self.base, "GITHUB_OUTPUT": str(output)},
            check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=15,
        )
        self.assertEqual(output.read_text(), f"code-changed={str(expected).lower()}\n")

    def test_push_includes_code_before_the_last_documentation_commit(self):
        self.commit("crates/arany/src/changed.rs")
        self.commit("docs/guide.md")
        self.detect("push", True)

    def test_documentation_push_skips_tests(self):
        self.commit("docs/guide.md")
        self.detect("push", False)

    def test_pull_request_diff_excludes_changes_only_in_its_base(self):
        self.git("switch", "-c", "topic")
        self.commit("docs/guide.md")
        self.git("switch", "main")
        self.commit("crates/arany/src/base.rs")
        self.git("merge", "--no-ff", "topic", "-m", "Merge fixture")
        self.detect("pull_request", False)

    def test_moving_code_into_docs_still_runs_tests(self):
        self.commit("crates/arany/fixture.md")
        self.base = self.git("rev-parse", "HEAD").strip()
        (self.repo / "docs").mkdir()
        self.git("mv", "crates/arany/fixture.md", "docs/fixture.md")
        self.git("commit", "-m", "Move fixture")
        self.detect("push", True)


if __name__ == "__main__":
    unittest.main()
