#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Fail if a comment or doc breaks the part of CONTRIBUTING.md's comment
# policy a grep can see. In every tracked file outside docs/review/ (the
# frozen records):
#
#   (a) a line reference into our own code: a tracked source's repo path,
#       or a basename that is ours alone, followed by `:<digit>`
#       (`device/src/nvidia/v1.rs:525`, `nvgpu_xfer.c:1849`). Cite the
#       symbol instead. Basenames that also name files of other projects
#       (mod.rs, lib.rs, ...) count only with a path in front of them.
#
# and in every tracked file but Markdown (code, scripts, units, rules):
#
#   (b) a review round named in a comment (`review 2026-09-29`, `the
#       2026-09-29 review`, `(S3, 2026-09-29)`): say what
#       the code does and why; the rounds are SECURITY.md's appendices;
#   (c) a section of one of our docs cited by number
#       (`SECURITY.md §18`): cite it by name, `SECURITY.md, "Capture
#       injection"`, which survives a renumbering.
#
# An exception goes in ALLOW below, as `path:needle` with the reason
# beside it; the needle must occur on the offending line (an empty needle
# excepts the file). The two there are the files that quote the patterns.
#
# Usage: scripts/check-comments.sh   (exit 0 clean, 1 with a list of findings)
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

exec python3 - "$@" <<'PY'
import os
import re
import subprocess
import sys

# path:needle -> reason. An empty needle excepts the whole file.
ALLOW = {
    "scripts/check-comments.sh:": "quotes the patterns it refuses, as examples",
    "CONTRIBUTING.md:": "quotes the patterns the policy refuses, as examples",
}

# Basenames of ours that are also the names of files in the projects the
# comments cite (the kernel, NVIDIA's modules, crosvm, nesbox, crates): a
# bare `mod.rs:12` is more likely theirs. With a path in front, still ours.
GENERIC = {
    "mod.rs", "lib.rs", "main.rs", "build.rs", "tests.rs", "sys.rs",
    "error.rs", "fd.rs", "mem.rs", "net.rs", "proc.rs", "ioctl.rs",
    "host.rs", "queue.rs", "handler.rs", "vm.rs", "memslot.rs",
    "config.rs", "util.rs", "types.rs", "device.rs", "fence.rs",
    "backend.rs", "server.rs", "check.rs", "registry.rs", "fake.rs",
    "session.rs", "log.rs", "frame.rs", "shm.rs", "Makefile",
}

files = subprocess.run(["git", "ls-files", "-z"], check=True,
                       capture_output=True).stdout.decode().split("\0")
files = [f for f in files if f and not f.startswith("docs/review/")]

SRC_EXT = (".rs", ".c", ".h", ".py", ".sh", ".nix")
ours = [f for f in files if f.endswith(SRC_EXT)]
paths = set(ours)
basenames = {}
for f in ours:
    basenames.setdefault(os.path.basename(f), []).append(f)
names = {b for b in basenames if b not in GENERIC}

REF = re.compile(r"([A-Za-z0-9_./+-]+\.(?:rs|c|h|py|sh|nix)):[0-9]")
REVIEW = re.compile(
    r"review 20[0-9][0-9]-|20[0-9][0-9]-[0-9][0-9]-[0-9][0-9] review"
    r"|\([A-Z]+[0-9]+, 20[0-9][0-9]-[0-9][0-9]-[0-9][0-9]\)")
SECTION = re.compile(
    r"(SECURITY|ARCHITECTURE|DEPLOY|TESTING|BENCHMARKS|README)(\.md)?[^§\n]{0,20}§ ?[0-9]")

def skip_binary(path):
    try:
        with open(path, "rb") as fh:
            return b"\0" in fh.read(8192)
    except OSError:
        return True

findings = []

def allowed(path, line):
    for key in ALLOW:
        p, _, needle = key.partition(":")
        if p == path and needle in line:
            return True
    return False

for path in files:
    if not os.path.isfile(path) or skip_binary(path):
        continue
    md = path.endswith(".md")
    # Generated tables quote NVIDIA's names, not our comments.
    if path.endswith(".json") or path == "gen/src/rmallow/generated.rs":
        continue
    with open(path, encoding="utf-8", errors="replace") as fh:
        for n, line in enumerate(fh, 1):
            if allowed(path, line):
                continue
            for m in REF.finditer(line):
                ref = m.group(1)
                # A path relative to the repo, or `./path`, is ours if tracked.
                rel = ref[2:] if ref.startswith("./") else ref
                if "/" in rel:
                    # A partial path (`src/nvidia/v1.rs`) is ours if it ends
                    # one of our paths, unless its name is generic too
                    # (crosvm's `vm_control/src/lib.rs`, a crate's `src/lib.rs`).
                    hit = rel in paths or (
                        os.path.basename(rel) not in GENERIC
                        and any(p.endswith("/" + rel) for p in ours))
                else:
                    hit = rel in names
                if hit:
                    findings.append(f"{path}:{n}: line reference into our own code ({ref}:...): cite the symbol")
            if md:
                continue
            if REVIEW.search(line):
                findings.append(f"{path}:{n}: a review round in a comment: say what the code does")
            if SECTION.search(line):
                findings.append(f"{path}:{n}: a doc section cited by number: cite it by name")

if findings:
    print("\n".join(findings), file=sys.stderr)
    print(f"comment policy: {len(findings)} finding(s) (CONTRIBUTING.md)", file=sys.stderr)
    sys.exit(1)
PY
