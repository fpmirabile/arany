import os
import re
import subprocess

NAME = re.compile(rb"\binclude(?:_str|_bytes)?\b")
STRING = re.compile(rb'r(#*)"(.*?)"\1|"((?:[^"\\]|\\.)*)"', re.S)
ESCAPE = re.compile(rb"\\(?:x([0-9a-fA-F]{2})|u\{([0-9a-fA-F_]+)\}|(\r?\n[\t\n\r ]*)|(.))", re.S)
SIMPLE_ESCAPES = {b"n": b"\n", b"r": b"\r", b"t": b"\t", b"\\": b"\\", b"0": b"\0", b"'": b"'", b'"': b'"'}
SPACE = re.compile(rb"(?:[\t-\r ]|\xc2\x85|\xe2\x80[\x8e\x8f\xa8\xa9])*")
NESTING = re.compile(rb"/\*|\*/")
OPENERS = (b"(", b"[", b"{")
CLOSERS = (b")", b"]", b"}")
MAX_SOURCE_BYTES = 4 << 20
MAX_SOURCE_FILES = 10_000
MAX_SCANNED_BYTES = 256 << 20


def raise_error(error):
    raise error


def unescape(raw):
    def decode(match):
        if match[1]:
            return bytes([int(match[1], 16)])
        if match[2]:
            return chr(int(match[2].replace(b"_", b""), 16)).encode()
        if match[3] is not None:
            return b""
        return SIMPLE_ESCAPES[match[4]]

    return ESCAPE.sub(decode, raw)


def literal(match):
    return match[2] if match[3] is None else unescape(match[3])


def manifest_dir(directory):
    while not os.path.exists(os.path.join(directory, b"Cargo.toml")):
        parent = os.path.dirname(directory)
        if parent == directory:
            raise ValueError("no Cargo.toml above " + os.fsdecode(directory))
        directory = parent
    return directory


def skip(text, position):
    while True:
        position = SPACE.match(text, position).end()
        if text.startswith(b"//", position):
            end = text.find(b"\n", position)
            position = len(text) if end < 0 else end
        elif text.startswith(b"/*", position):
            depth, position = 1, position + 2
            while depth:
                token = NESTING.search(text, position)
                if not token:
                    raise ValueError("unterminated block comment")
                depth += 1 if token[0] == b"/*" else -1
                position = token.end()
        else:
            return position


def call(text, position, name, openers=OPENERS):
    if not text.startswith(name, position):
        return None
    position = skip(text, position + len(name))
    if text[position:position + 1] != b"!":
        return None
    position = skip(text, position + 1)
    if text[position:position + 1] not in openers:
        raise ValueError("unsupported include delimiter")
    return skip(text, position + 1)


def argument(text, position):
    match = STRING.match(text, position)
    if match:
        return literal(match), skip(text, match.end())
    after = call(text, position, b"env", (b"(",))
    match = STRING.match(text, after) if after is not None else None
    if not match:
        raise ValueError("unsupported include argument")
    position = skip(text, match.end())
    if text[position:position + 1] != b")":
        raise ValueError("unsupported include argument")
    return literal(match).decode(), skip(text, position + 1)


def include_target(text, position, directory):
    parts = []
    after = call(text, position, b"concat")
    if after is None:
        part, _ = argument(text, position)
        if isinstance(part, str):
            raise ValueError("unsupported include argument")
        return os.path.normpath(os.path.join(directory, part))
    position = after
    while text[position:position + 1] not in CLOSERS:
        part, position = argument(text, position)
        parts.append(part)
        if text[position:position + 1] == b",":
            position = skip(text, position + 1)
        elif text[position:position + 1] not in CLOSERS:
            raise ValueError("unsupported include argument")
    if "OUT_DIR" in parts:
        return None
    if any(isinstance(part, str) and part != "CARGO_MANIFEST_DIR" for part in parts):
        raise ValueError("unsupported include environment variable")
    root = manifest_dir(directory) if "CARGO_MANIFEST_DIR" in parts else b""
    path = b"".join(root if isinstance(part, str) else part for part in parts)
    return os.path.normpath(path if root else os.path.join(directory, path))


def included_paths():
    included, files, scanned = set(), 0, 0
    if not os.path.isdir(b"crates"):
        return included
    for directory, directories, names in os.walk(b"crates", onerror=raise_error):
        for name in sorted(directories + names):
            link = os.path.join(directory, name)
            if os.path.islink(link) and (name in directories or name.endswith(b".rs")):
                raise ValueError("symlinked Rust source " + os.fsdecode(link))
        for name in sorted(names):
            source = os.path.join(directory, name)
            if not name.endswith(b".rs"):
                continue
            with open(source, "rb") as file:
                text = file.read(MAX_SOURCE_BYTES + 1)
            files += 1
            scanned += len(text)
            if len(text) > MAX_SOURCE_BYTES or files > MAX_SOURCE_FILES or scanned > MAX_SCANNED_BYTES:
                raise ValueError("Rust source scan exceeds its bounds")
            text = text.replace(b"\r\n", b"\n")
            for match in NAME.finditer(text):
                position = call(text, match.start(), match[0])
                if position is None:
                    continue
                target = include_target(text, position, directory)
                if target is not None:
                    included.add(target)
    return included


def non_code(path, included):
    if path in included or path.startswith((b".github/workflows/", b".github/actions/", b"scripts/")):
        return False
    if path.startswith(b"crates/"):
        return path.endswith(b".md")
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
    if not paths:
        return True
    included = included_paths()
    return any(not non_code(path, included) for path in paths)


def main():
    changed = code_changed(os.environ.get("CI_EVENT", ""), os.environ.get("CI_BEFORE", ""))
    with open(os.environ["GITHUB_OUTPUT"], "a") as output:
        output.write(f"code-changed={str(changed).lower()}\n")


if __name__ == "__main__":
    main()
