# SPDX-License-Identifier: Apache-2.0
"""Compare a unit of contrib/systemd with the one nix/module.nix renders.

    python3 unit-diff.py CONTRIB_UNIT MODULE_UNIT [KEY,KEY...]

(the optional list: more keys whose values differ by design, as for a
template compared with one of its instances)

The module restates contrib/systemd's units in Nix; this fails when the two
say different things, key by key, in [Unit], [Service] and [Socket]. What
differs by design is left out: the paths of what they run and may execute,
the environment (the module sets it per VM, and NixOS adds its own), and the
module's own settings for the resource bounds. Values are compared as sets of
words, with yes/true and no/false the same, so that one line with a list and
several lines of it compare equal.
"""

import sys

# Keys whose values differ by design (their presence is still compared).
VALUE_DIFFERS = {
    "ExecStart",  # the package's path; the module's extraArgs
    "ExecPaths",  # /usr/bin/... and the libraries there, or /nix/store
    "Environment",
    "MemoryMax",  # services.virtio-nvgpu.memoryMax
    "TasksMax",  # services.virtio-nvgpu.tasksMax
}
# Keys only one side has, by design.
ONE_SIDE = {
    "EnvironmentFile",  # contrib: /etc/virtio-nvgpu/vmN.env; the module sets it in Nix
    "X-StopIfChanged",
    "X-RestartIfChanged",
    "X-StopOnRemoval",
    "X-OnlyManualStart",
}
SECTIONS = {"Unit", "Service", "Socket"}
BOOL = {"yes": "true", "on": "true", "1": "true", "no": "false", "off": "false", "0": "false"}


def parse(path):
    keys = {}
    section = None
    with open(path) as f:
        for raw in f:
            line = raw.strip()
            if not line or line.startswith(("#", ";")):
                continue
            if line.startswith("[") and line.endswith("]"):
                section = line[1:-1]
                continue
            if section not in SECTIONS or "=" not in line:
                continue
            k, v = line.split("=", 1)
            k, v = k.strip(), v.strip()
            if k in ONE_SIDE:
                continue
            words = keys.setdefault((section, k), set())
            if v == "":
                words.clear()  # an empty assignment resets a list
                words.add("")
                continue
            words.discard("")
            for w in v.split():
                words.add(BOOL.get(w.strip('"').lower(), w.strip('"')))
    return keys


def main():
    if len(sys.argv) > 3:
        VALUE_DIFFERS.update(k for k in sys.argv[3].split(",") if k)
    contrib, module = parse(sys.argv[1]), parse(sys.argv[2])
    bad = []
    for key in sorted(set(contrib) | set(module)):
        section, k = key
        if key not in contrib:
            bad.append(f"[{section}] {k}: only in the module ({' '.join(sorted(module[key]))})")
        elif key not in module:
            bad.append(f"[{section}] {k}: only in contrib ({' '.join(sorted(contrib[key]))})")
        elif k not in VALUE_DIFFERS and contrib[key] != module[key]:
            bad.append(
                f"[{section}] {k}: contrib {' '.join(sorted(contrib[key]))!r}, "
                f"module {' '.join(sorted(module[key]))!r}"
            )
    for b in bad:
        print(f"{sys.argv[1]}: {b}", file=sys.stderr)
    sys.exit(1 if bad else 0)


main()
