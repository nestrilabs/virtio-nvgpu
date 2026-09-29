# Vendored protocol XML

`build.rs` generates the codec's tables from every `*.xml` here, and checks the
allowlist in `src/policy_table.rs` against them: every global allowed there must
be defined here, and every descriptor any object reachable from it can carry
must have a class. Each file keeps its own copyright and licence header.

| source | revision | files |
|---|---|---|
| wayland (`protocol/wayland.xml`) | 381af21 | `wayland.xml` |
| wayland-protocols | 819004a | stable: linux-dmabuf, presentation-time, tablet, viewporter, xdg-shell; staging: alpha-modifier, color-management, commit-timing, content-type, cursor-shape, drm-lease, ext-background-effect, ext-foreign-toplevel-list, ext-idle-notify, ext-image-capture-source, ext-image-copy-capture, fifo, fractional-scale, linux-drm-syncobj, single-pixel-buffer, tearing-control, xdg-activation, xdg-dialog, xdg-system-bell, xdg-toplevel-tag; unstable: idle-inhibit, keyboard-shortcuts-inhibit, pointer-constraints, pointer-gestures, primary-selection, relative-pointer, text-input v1/v3, xdg-decoration, xdg-foreign v2, xdg-output |
| hyprland-protocols | cc9a8fd (0.7.1, what Hyprland 0.56 pins) | `hyprland-surface-v1.xml` |
| Hyprland `protocols/` | `main` at e368c13c (0.56.0 + 203 commits) | `kde-server-decoration.xml` (LGPL-2.1-or-later, as its header says; [`NOTICE`](../../NOTICE)) |

Three files define globals that are *not* allowed: `ext-image-copy-capture`,
`ext-image-capture-source` and `ext-foreign-toplevel-list` (capture, which
`src/policy_table.rs` leaves out deliberately, and the list of every host
window's title and app id). Nothing in the proxy names them: the
value rewrite that once did (`dmabuf_device`) was removed with the rest of
what the allowlist cannot reach (5201973), and they are kept only so that the
codec can parse them if a policy line ever allows them. The closure check
covers what is allowed, never these.

Upgrading: re-copy the files, rebuild (the closure check says what needs a
policy line), and look at what the new versions added before raising a cap in
`GLOBALS` -- a version is only offered up to what the XML here knows, so a newer
compositor cannot hand a guest messages the proxy cannot parse.
