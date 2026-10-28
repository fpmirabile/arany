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
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        tests = Path(directory.name) / "crates/arany-cli/tests"
        tests.mkdir(parents=True)
        (tests / "generate_schema.rs").write_text(
            'const A: &[u8] = include_bytes!("../../../docs/specs/limits.md");\n'
            'const B: &str = include_str!("fixtures/golden.md");\n'
        )
        previous = os.getcwd()
        os.chdir(directory.name)
        self.addCleanup(os.chdir, previous)

    def test_paths(self):
        non_code = [
            b"README.md", b"nested/guide.md", b"docs/diagram.svg",
            b"planning/plan.json", b"agents/skills/check.py", b".agents/skills/a/SKILL.md",
            b".claude/settings.json",
            b".github/ISSUE_TEMPLATE/bug.yml", b".github/pull_request_template.md",
            b"skills-lock.json", b".github/dependabot.yml", b"docs/spaces and\nnewlines.txt", b"docs/non-utf8-\xff",
            b"crates/arany/README.md", b"crates/arany-cli/tests/guard_runs/latency.md",
            b"crates/arany-cli/tests/fixtures/acp/README.md", b"crates/a/docs/guide.md",
        ]
        code = [
            b"scripts/notes.md",
            b"crates/new/Cargo.toml", b"crates/arany/src/lib.rs", b"crates/arany-cli/tests/fixtures/x.json",
            b"docs/specs/limits.md", b"crates/arany-cli/tests/fixtures/golden.md", b"Cargo.toml", b"Cargo.lock", b"build.rs",
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


class IncludedPaths(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.src = self.root / "crates/a/src"
        self.src.mkdir(parents=True)
        (self.root / "crates/a/Cargo.toml").write_text("")
        previous = os.getcwd()
        os.chdir(directory.name)
        self.addCleanup(os.chdir, previous)

    def included(self, source):
        (self.src / "lib.rs").write_text(source)
        return {os.fsdecode(path) for path in DETECTOR["included_paths"]()}

    def test_literal_forms_and_delimiters(self):
        source = (
            'include_str!(r"raw.md"); include_str!(r#"hash".md"#); include_bytes!("a\\x62.md");\n'
            'include_str!["sq.md"]; include_str!{ "brace.md" }; std::include!("../up.rs");\n'
            'include_str!("line\\\n   cont.md"); include_str!("\\u{63}.md");\n'
            'include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/build.rs"));\n'
            'include_bytes!(concat!(env!("OUT_DIR"), "/generated.bin"));\n'
            'include_str!(concat!("fixtures/", "concat.md"));\n'
            'include_str /* a */ ! // b\n /* c */ ( /* d */ "comment.md" /* e */ );\n'
            'include_str!(concat /* a */ ! /* b */ ( /* c */ "x/" /* d */ , /* e */ "y.md" /* f */ , ));\n'
        )
        base = "crates/a/src/"
        self.assertEqual(self.included(source), {
            base + "raw.md", base + 'hash".md', base + "ab.md", base + "sq.md",
            base + "brace.md", "crates/a/up.rs", base + "linecont.md", base + "c.md",
            "crates/a/build.rs", base + "fixtures/concat.md", base + "comment.md", base + "x/y.md",
        })

    def test_literal_concat_change_runs_tests(self):
        (self.src / "lib.rs").write_text('include_str!(concat!("fixtures/", "golden.md"));')
        with patch("subprocess.check_output", return_value=b"crates/a/src/fixtures/golden.md\0"), patch.dict(
            os.environ, {"CI_EVENT": "pull_request"}
        ):
            self.assertTrue(DETECTOR["code_changed"]("pull_request", ""))

    def test_unsupported_syntax_fails_closed(self):
        for source in [
            'include_str!(PATH);', 'include_str!(concat!(env!("HOME"), "/x"));',
            'include_str!(concat!(stringify!(x)));', 'include_str!("bad\\q.md");',
        ]:
            with self.subTest(source=source), self.assertRaises((ValueError, KeyError)):
                self.included(source)

    def test_nested_block_comment_is_resolved(self):
        (self.src / "lib.rs").write_text('include_str /* outer /* inner */ tail */ !("fixtures/golden.md");')
        with patch("subprocess.check_output", return_value=b"crates/a/src/fixtures/golden.md\0"):
            self.assertTrue(DETECTOR["code_changed"]("pull_request", ""))

    def test_comment_hidden_literal_is_not_the_target(self):
        (self.src / "lib.rs").write_text('include_str!(/* outer /* inner */ "wrong.md" */ "fixtures/golden.md");')
        self.assertEqual(self.included(
            'include_str!(/* outer /* inner */ "wrong.md" */ "fixtures/golden.md");'
        ), {"crates/a/src/fixtures/golden.md"})
        with patch("subprocess.check_output", return_value=b"crates/a/src/fixtures/golden.md\0"):
            self.assertTrue(DETECTOR["code_changed"]("pull_request", ""))

    def test_unicode_whitespace_between_tokens(self):
        for space in ["\u200e", "\u2028", "\u0085"]:
            with self.subTest(space=repr(space)):
                (self.src / "lib.rs").write_text(f'include_str!{space}("fixtures/golden.md");', encoding="utf-8")
                with patch("subprocess.check_output", return_value=b"crates/a/src/fixtures/golden.md\0"):
                    self.assertTrue(DETECTOR["code_changed"]("pull_request", ""))

    def test_crlf_in_literals_is_normalized(self):
        for source in [b'include_str!("fixtures/golden\r\n.md");', b'include_str!(r"fixtures/golden\r\n.md");']:
            with self.subTest(source=source):
                (self.src / "lib.rs").write_bytes(source)
                with patch("subprocess.check_output", return_value=b"crates/a/src/fixtures/golden\n.md\0"):
                    self.assertTrue(DETECTOR["code_changed"]("pull_request", ""))

    def test_continuation_preserves_vertical_tab_and_form_feed(self):
        for space in [b"\x0b", b"\x0c"]:
            with self.subTest(space=space):
                (self.src / "lib.rs").write_bytes(b'include_str!("fixtures/\\\n' + space + b'golden.md");')
                path = b"crates/a/src/fixtures/" + space + b"golden.md\0"
                with patch("subprocess.check_output", return_value=path):
                    self.assertTrue(DETECTOR["code_changed"]("pull_request", ""))

    def test_symlinked_sources_fail_closed(self):
        (self.root / "real.rs").write_text('include_str!("../../../docs/golden.md");')
        (self.root / "realdir").mkdir()
        for link, target in [(self.src / "lib.rs", self.root / "real.rs"), (self.root / "crates/b", self.root / "realdir")]:
            with self.subTest(link=link.name):
                link.unlink(missing_ok=True)
                link.symlink_to(target)
                with patch("subprocess.check_output", return_value=b"docs/golden.md\0"), self.assertRaises(ValueError):
                    DETECTOR["code_changed"]("pull_request", "")
                link.unlink()

    def test_unparsed_include_fails_closed(self):
        for source in ['include_str!(x);', 'include_str!(/* open "a.md");', 'include_str!(concat!("a" "b"));', 'include_str!\u2060("a.md");']:
            with self.subTest(source=source), self.assertRaises(ValueError):
                self.included(source)

    def test_oversized_source_fails_closed(self):
        with self.assertRaises(ValueError):
            self.included('include_str!("a.md");' + " " * (4 << 20))

    def test_traversal_error_fails_closed(self):
        locked = self.root / "crates/locked"
        locked.mkdir()
        locked.chmod(0)
        self.addCleanup(locked.chmod, 0o700)
        if os.access(locked, os.R_OK):
            self.skipTest("permissions are not enforced")
        with self.assertRaises(OSError):
            self.included("")


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

    def commit(self, path, text=None):
        file = self.repo / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text(text or path)
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

    def test_markdown_under_crates_skips_tests(self):
        self.commit("crates/arany-cli/tests/guard_runs/latency.md")
        self.detect("push", False)

    def test_included_document_runs_tests(self):
        self.commit(
            "crates/arany-cli/tests/generate_schema.rs",
            'include_bytes!("../../../docs/specs/limits.md");',
        )
        self.base = self.git("rev-parse", "HEAD").strip()
        self.commit("docs/specs/limits.md")
        self.detect("push", True)

    def test_moving_code_into_docs_still_runs_tests(self):
        self.commit("crates/arany/fixture.rs")
        self.base = self.git("rev-parse", "HEAD").strip()
        (self.repo / "docs").mkdir()
        self.git("mv", "crates/arany/fixture.rs", "docs/fixture.rs")
        self.git("commit", "-m", "Move fixture")
        self.detect("push", True)


if __name__ == "__main__":
    unittest.main()
