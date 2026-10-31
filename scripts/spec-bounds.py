# Matching prose bounds require a limits.md#anchor link (../limits.md#anchor from a
# domain folder), or immediately following
# VALUE UNIT<!-- unrelated-bound: reason --> identifying a genuinely distinct bound.
import argparse
import re
import sys
from decimal import Decimal
from pathlib import Path


SPEC_DIR = Path("docs/specs")
LIMITS_FILE = SPEC_DIR / "limits.md"
NUMBER = re.compile(r"(?<!\w)(\d[\d,]*(?:\.\d+)?)(?:\s*-\s*|\s*)([A-Za-z][A-Za-z /\n-]*)")
UNIT_WORD = re.compile(r"[A-Za-z]+(?:/[A-Za-z]+)?")
FENCE = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")
GENERATED_LIMIT_LINK = re.compile(
    r"\[(?P<label>[^\]]+)\]\((?P<prefix>\./|(?:\.\./)+)?limits\.md#(?P<anchor>[^)]+)\)"
)
HTML_COMMENT = re.compile(r"<!--.*?-->", re.DOTALL)
UNRELATED_MARKER = re.compile(r"[ \t]*<!-- unrelated-bound: (?P<reason>[^<>\r\n]+) -->")
UNIT_ALIASES = {
    "byte": ("bytes", 1),
    "bytes": ("bytes", 1),
    "b": ("bytes", 1),
    "kib": ("bytes", 1024),
    "mib": ("bytes", 1024**2),
    "gib": ("bytes", 1024**3),
    "ms": ("milliseconds", 1),
    "millisecond": ("milliseconds", 1),
    "milliseconds": ("milliseconds", 1),
    "second": ("milliseconds", 1000),
    "seconds": ("milliseconds", 1000),
    "minute": ("milliseconds", 60_000),
    "minutes": ("milliseconds", 60_000),
}


def canonical_unit(unit):
    normalized = " ".join(unit.lower().split())
    if normalized in UNIT_ALIASES:
        return UNIT_ALIASES[normalized]
    return (normalized.removesuffix("s"), 1)


def generated_bounds(path):
    bounds = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.startswith("| <a id="):
            continue
        columns = [column.strip() for column in line.strip("|").split("|")]
        anchor = re.search(r'<a id="([^"]+)"', columns[0]).group(1)
        value = Decimal(columns[1].replace(",", ""))
        unit, scale = canonical_unit(columns[2])
        bounds.setdefault((value * scale, unit), set()).add(anchor)
    return bounds


def value_unit_mentions(text, bounds):
    units = {unit for _, unit in bounds}
    unit_words = max(len(unit.split()) for unit in units)
    for match in NUMBER.finditer(text):
        value = Decimal(match.group(1).replace(",", ""))
        words = list(UNIT_WORD.finditer(match.group(2)))
        prefix_words = min(len(words), unit_words)
        for count in range(1, prefix_words + 1):
            unit_text = " ".join(word.group() for word in words[:count])
            unit, scale = canonical_unit(unit_text)
            pair = (value * scale, unit)
            if unit in units or count == prefix_words:
                end = match.start(2) + words[count - 1].end()
                yield match.start(1), end, f"{match.group(1)} {unit_text}", pair
                break


def prose_blocks(text):
    fence_marker = None
    paragraph = []
    first_line = None
    for line_number, line in enumerate(text.splitlines(), 1):
        fence = FENCE.match(line)
        if fence_marker is not None:
            if fence:
                marker = fence.group(1)
                if marker[0] == fence_marker[0] and len(marker) >= len(fence_marker) and not fence.group(2).strip():
                    fence_marker = None
            continue
        if fence and not (fence.group(1)[0] == "`" and "`" in fence.group(2)):
            if paragraph:
                yield first_line, "\n".join(paragraph)
                paragraph.clear()
                first_line = None
            fence_marker = fence.group(1)
            continue
        if first_line is None:
            first_line = line_number
        paragraph.append(line)
    if paragraph:
        yield first_line, "\n".join(paragraph)


def restatement_errors(path, text, bounds):
    errors = []
    targets = {anchor: pair for pair, anchors in bounds.items() for anchor in anchors}
    depth = len(path.parent.relative_to(SPEC_DIR).parts)
    prefixes = {"../" * depth} if depth else {"", "./"}
    for first_line, paragraph in prose_blocks(text):
        comments = list(HTML_COMMENT.finditer(paragraph))
        links = [
            link for link in GENERATED_LIMIT_LINK.finditer(paragraph)
            if (link.group("prefix") or "") in prefixes
            and not any(comment.start() <= link.start() < comment.end() for comment in comments)
        ]
        for link in links:
            line = first_line + paragraph[:link.start()].count("\n")
            anchor = link.group("anchor")
            if anchor not in targets:
                errors.append(f"{path}:{line}: unknown generated limit anchor {anchor}")
                continue
            for _, _, mention, pair in value_unit_mentions(link.group("label"), bounds):
                if pair != targets[anchor]:
                    errors.append(f"{path}:{line}: generated limit label {mention} does not match {anchor}")
        for start, end, mention, pair in value_unit_mentions(paragraph, bounds):
            if pair not in bounds:
                continue
            if any(comment.start() <= start < comment.end() for comment in comments):
                continue
            if any(
                link.start("label") <= start and end <= link.end("label")
                for link in links
            ):
                continue
            marker = UNRELATED_MARKER.match(paragraph, end)
            if marker and marker.group("reason").strip():
                continue
            line = first_line + paragraph[:start].count("\n")
            errors.append(f"{path}:{line}: restates generated bound {mention}; link to {LIMITS_FILE}")
    return errors


def check(root):
    bounds = generated_bounds(root / LIMITS_FILE)
    errors = []
    for path in sorted((root / SPEC_DIR).rglob("*.md")):
        if path != root / LIMITS_FILE:
            text = path.read_text(encoding="utf-8")
            errors.extend(restatement_errors(path.relative_to(root), text, bounds))
    return errors


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    errors = check(args.root)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("Generated-bound references are valid.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
