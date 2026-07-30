#!/usr/bin/env python3
"""Fail CI when tracked files appear to contain provider credentials.

The scanner deliberately reports only file names, line numbers, and rule IDs. It
never prints the matching line or candidate value, so a failure cannot copy a
credential into CI logs.
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

MAX_FILE_BYTES = 2 * 1024 * 1024
ALLOWED_PLACEHOLDER_PREFIXES = (
    "${",
    "$(",
    "{{",
    "<",
    "replace",
    "your_",
    "your-",
    "dummy",
    "test",
    "redacted",
    "example",
    "secretkeyref",
    "valuefrom",
)

# Build the well-known prefix without embedding a contiguous credential prefix
# in this scanner's own source.
OPENAI_PREFIX = "s" + "k-"
OPENAI_KEY_PATTERN = re.compile(
    r"\b" + re.escape(OPENAI_PREFIX) + r"(?:proj-|svcacct-)?[A-Za-z0-9_-]{20,}\b"
)
OPENAI_ASSIGNMENT_PATTERN = re.compile(
    r"^\s*(?:export\s+)?OPENAI_API_KEY\s*[:=]\s*(.*?)\s*(?:#.*)?$",
    re.IGNORECASE,
)


def repo_root() -> Path:
    result = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"],
        check=True,
        capture_output=True,
        text=True,
    )
    return Path(result.stdout.strip())


def tracked_files(root: Path) -> list[Path]:
    result = subprocess.run(
        ["git", "ls-files", "-z"],
        cwd=root,
        check=True,
        capture_output=True,
    )
    return [root / raw.decode("utf-8") for raw in result.stdout.split(b"\0") if raw]


def normalized_assignment_value(raw: str) -> str:
    value = raw.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {"'", '"'}:
        value = value[1:-1].strip()
    return value


def assignment_is_safe(raw: str) -> bool:
    value = normalized_assignment_value(raw)
    if not value:
        return True
    return value.lower().startswith(ALLOWED_PLACEHOLDER_PREFIXES)


def scan_file(path: Path) -> list[tuple[int, str]]:
    try:
        data = path.read_bytes()
    except OSError:
        return []
    if len(data) > MAX_FILE_BYTES or b"\0" in data:
        return []

    text = data.decode("utf-8", errors="replace")
    findings: list[tuple[int, str]] = []
    for line_number, line in enumerate(text.splitlines(), start=1):
        if OPENAI_KEY_PATTERN.search(line):
            findings.append((line_number, "OPENAI_KEY_MATERIAL"))
        assignment = OPENAI_ASSIGNMENT_PATTERN.match(line)
        if assignment and not assignment_is_safe(assignment.group(1)):
            findings.append((line_number, "PLAINTEXT_OPENAI_API_KEY"))
    return findings


def main() -> int:
    root = repo_root()
    failures: list[tuple[Path, int, str]] = []

    for path in tracked_files(root):
        relative = path.relative_to(root)
        name = relative.name
        if (name == ".env" or name.startswith(".env.")) and name != ".env.example":
            failures.append((relative, 1, "TRACKED_ENV_FILE"))
        failures.extend((relative, line, rule) for line, rule in scan_file(path))

    if not failures:
        print("provider secret audit passed")
        return 0

    print(
        "provider secret audit failed; candidate values are intentionally suppressed",
        file=sys.stderr,
    )
    for path, line, rule in failures:
        print(f"{path}:{line}: {rule}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
