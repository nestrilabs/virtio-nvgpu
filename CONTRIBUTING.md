# Contributing

## Comments and documentation

A comment or a doc says what the code does, and why. Code and text move
under it; the rules below keep a reference from pointing at the wrong thing
when they do.

**Our own code: a symbol, never a line.** Name the file and the function or
type: `nvidia/mod.rs NvidiaBackend::serve`, `nvgpu_rmio.c
nvgpu_ioctl_rm_control()`. A line number is wrong after the next edit above
it, and nothing says so.

**Our own docs: a section by name, never by number.** Write
`SECURITY.md, "Capture injection"` or `ARCHITECTURE.md, "Fences"`. A
renumbering then breaks nothing. A finding id from SECURITY.md's finding
index (Appendix C) may be cited on its own, as the threat a check answers:
`(S-6)`, `(R3)`.

**External sources: the project and its release.** Name the project, the
release or version, the path and the function:
"open-gpu-kernel-modules 595.99.02, kernel-open/nvidia/nv.c
nvidia_ioctl()", "Linux 7.2.7, drivers/gpu/drm/drm_ioctl.c drm_ioctl()". A
line number is allowed with that pin, never without it. One pin at the top
of a file covers the file's references to that project. Use a path, not a
bare name, where the name is ambiguous (`os.c`, `mem.c`, `fence.c`). Name
the function too, so the reference survives a release change. A crate is
cited with its version: `vhost-user-backend 0.23.0, src/handler.rs`.

**The code as it is, not its history.** A comment does not say which
review found what, or what the code used to do. What went wrong before is
worth saying only as the reason for what the code does now, and then in the
present tense: "a plain `+` here would let one app abort the backend", not
"the 2026-09-29 review found ...". The history lives in SECURITY.md's
appendices and in the commit log.

**Frozen records.** `docs/review/*` are the records of reviews at the
commit their headers name. Their references are to that commit, and they
are not updated.

## What CI checks

`scripts/ci.sh fast` runs `scripts/check-comments.sh`, which fails on:

- a line reference into our own code (`device/src/nvidia/v1.rs:525`,
  `nvgpu_xfer.c:1849`), anywhere outside `docs/review/`, Markdown included;
- a review round named in a comment (`review 2026-09-29`, `the 2026-09-29
  review`, `(S3, 2026-09-29)`), in any file but Markdown;
- a section of our docs cited by number (`SECURITY.md §18`), in any file
  but Markdown.

A basename that other projects' files also have (`mod.rs`, `lib.rs`,
`fence.rs`, ...) counts as ours only with a path in front of it; the list
is `GENERIC` in the script. The rest of the policy -- external references
pinned, comments in the present tense -- is for review, not for a grep.

An exception goes in the script's `ALLOW` table, as `path:needle`, with the
reason beside it. It holds this file and the script itself, which quote the
patterns as examples, and nothing else.
