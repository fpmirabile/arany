import os
import re
import subprocess


def non_code(path):
    if path.startswith((b".github/workflows/", b".github/actions/", b"crates/", b"scripts/")):
        return False
    return (
        path.endswith(b".md")
        or path.startswith((b"docs/", b"planning/", b"agents/", b".agents/", b".claude/", b".github/ISSUE_TEMPLATE/"))
        or path in (b"skills-lock.json", b".github/dependabot.yml")
    )


def code_changed(event, before):
    if event == "pull_request":
        base = "HEAD^1"
    elif event == "push" and re.fullmatch(r"[0-9a-f]{40}", before) and before != "0" * 40:
        base = before
        subprocess.run(
            ["git", "fetch", "--no-tags", "--depth=1", "origin", base],
            check=True,
            timeout=30,
        )
    else:
        return True
    paths = subprocess.check_output(
        ["git", "diff", "--no-renames", "--name-only", "-z", base, "HEAD"],
        timeout=30,
    ).split(b"\0")[:-1]
    return not paths or any(not non_code(path) for path in paths)


def main():
    changed = code_changed(os.environ.get("CI_EVENT", ""), os.environ.get("CI_BEFORE", ""))
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(f"code-changed={str(changed).lower()}\n")


if __name__ == "__main__":
    main()
