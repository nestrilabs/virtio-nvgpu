# SPDX-License-Identifier: Apache-2.0
"""Compare a unit of contrib/systemd with the one nix/module.nix installs,
and hold the module's drop-ins to the keys a drop-in may set.

    python3 unit-diff.py CONTRIB_UNIT MODULE_UNIT [KEY,KEY...]
    python3 unit-diff.py --dropin DROPIN SECTION:KEY[=VALUE],...,env:NAME,...

The first form fails when the two units say different things, key by key, in
[Unit], [Service] and [Socket] (the optional list: more keys whose values
differ by design). The module installs contrib/systemd's units with only
their paths changed (nix/units.nix), so what differs by design is the paths
of what they run and may execute. Values are compared as sets of words, with
yes/true and no/false the same, so that one line with a list and several
lines of it compare equal.

The second form fails when a drop-in the module renders sets a key not in
the list, or, where the list gives a value, any other value, or sets a
variable (Environment=) the list does not name with env:NAME: what is per
VM (the flags, the resource bounds, the Wayland socket) may be set there,
and nothing that would loosen the unit's hardening. NixOS's own keys (the
variables it gives every unit, its X- keys) are allowed.
"""

import sys

# Keys whose values differ by design (their presence is still compared).
VALUE_DIFFERS = {
    "ExecStart",  # the backend's path: /usr/bin, or the store
    "ExecStartPre",  # nvgpu-pci-snapshot from /usr/libexec, or the store
    "ExecPaths",  # the backend's path
}
# Keys only one side has, by design: NixOS's own.
ONE_SIDE = {
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


# What NixOS puts in every unit's environment.
NIXOS_ENV = {"PATH", "LOCALE_ARCHIVE", "TZDIR"}


def dropin(path, allowed, env):
    """The keys of `path` outside `allowed` ({(section, key): value or None})
    and the variables outside `env`."""
    bad = []
    section = None
    with open(path) as f:
        for raw in f:
            line = raw.strip()
            if not line or line.startswith(("#", ";")):
                continue
            if line.startswith("[") and line.endswith("]"):
                section = line[1:-1]
                continue
            k, v = (x.strip() for x in line.split("=", 1))
            if k in ONE_SIDE:
                continue
            if (section, k) == ("Service", "Environment"):
                name = v.strip('"').split("=", 1)[0]
                if name not in NIXOS_ENV | env:
                    bad.append(f"[{section}] {k}={v}: not a variable a drop-in may set")
                continue
            want = allowed.get((section, k), False)
            if want is False:
                bad.append(f"[{section}] {k}={v}: not a key a drop-in may set")
            elif want is not None and v != want:
                bad.append(f"[{section}] {k}={v}: only {k}={want} may be set")
    return bad


def main():
    if sys.argv[1] == "--dropin":
        allowed, env = {}, set()
        for item in sys.argv[3].split(","):
            sk, _, v = item.partition("=")
            sec, _, k = sk.partition(":")
            if sec == "env":
                env.add(k)
            else:
                allowed[(sec, k)] = v if "=" in item else None
        bad = dropin(sys.argv[2], allowed, env)
        for b in bad:
            print(f"{sys.argv[2]}: {b}", file=sys.stderr)
        sys.exit(1 if bad else 0)
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
