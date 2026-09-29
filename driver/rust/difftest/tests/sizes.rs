// SPDX-License-Identifier: GPL-2.0-only
//! A DRM-node caller's struct of another size than the native one: shorter
//! (older headers, the Steam runtime's 16-byte drm_syncobj_handle) or longer
//! (newer ones). The DRM node normalises the argument to the native command
//! as drm_ioctl() does (`nvgpu_drm_arg_in()`, drm_ioctl.c:848-915), finding
//! that command by type and number (`nvgpu_i2_native_cmd()`, in both
//! implementations); the interpreter itself then only ever takes the native
//! size, so no size a native caller could not send reaches the backend.

use nvgpu_guest_difftest::scen::{self, Call, DevSpec, Scenario, FDS};
use nvgpu_guest_difftest::world::{Ev, Hooks, World};

const ARG: u64 = 0x7f00_0000_0000;

const RENDER: u32 = 1;
const KMS: u32 = 2;
const MODESET: u32 = 3;

fn dev(version: &str) -> DevSpec {
    DevSpec {
        bad_schema: false,
        version: version.into(),
        v2: true,
        caps: 0,
        max_req: 1 << 20,
        max_resp: 1 << 20,
        fdt: vec![],
        handle: 5,
    }
}

fn world() -> World {
    World {
        fds: FDS.iter().copied().collect(),
        clock: Some(1_000_000),
        hooks: Hooks {
            mask: 0x3f,
            seed: 1,
            fail_gem: None,
        },
        ..World::default()
    }
}

fn ioc(dir: u32, ty: u8, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | (u32::from(ty) << 8) | nr
}

/// Every entry of every release's DRM table, at its own command and at
/// shorter, longer and other-direction ones: both implementations name the
/// same native command, the entry of exactly that command where there is one
/// (nvidia-drm's GRANT_PERMISSIONS has a typed and an untyped layout), else
/// the first with the number. Never for NVKMS (one command, told apart
/// inside it) or a number no entry has.
#[test]
fn a_callers_size_or_direction_is_normalised_to_the_schemas_command() {
    for version in [
        "535.129.03",
        "580.178.04",
        "595.71.05",
        "610.57.04",
        "615.71.09",
    ] {
        let d = dev(version);
        let set = scen::tables_for(&d);
        let mut seen = 0;
        for e in set.drm.ioctls {
            let sclass = u32::from(e.sclass);
            if sclass == MODESET {
                continue;
            }
            let size = (e.cmd >> 16) & 0x3fff;
            let dir = e.cmd >> 30;
            let with = |dir: u32, size: u32| (dir << 30) | (size << 16) | (e.cmd & 0xffff);
            let exact = |cmd: u32| {
                set.drm
                    .ioctls
                    .iter()
                    .any(|x| u32::from(x.sclass) == sclass && x.cmd == cmd)
            };
            let first = set
                .drm
                .ioctls
                .iter()
                .find(|x| u32::from(x.sclass) == sclass && x.cmd & 0xffff == e.cmd & 0xffff)
                .unwrap()
                .cmd;
            for cmd in [
                e.cmd,
                with(dir, size.saturating_sub(8)),
                with(dir, size.saturating_sub(1)),
                with(dir, 0),
                with(dir, size + 1),
                with(dir, size + 8),
                with(dir, 0x3fff),
                with(dir ^ 1, size),
                with(dir ^ 2, size),
                with(0, size),
            ] {
                let (c, r) = scen::native_cmd(&d, sclass, cmd);
                assert_eq!(c, r, "{version}: {:#x} as {cmd:#x}", e.cmd);
                let want = if exact(cmd) { cmd } else { first };
                assert_eq!(c, want, "{version}: {:#x} as {cmd:#x}", e.cmd);
            }
            // The other DRM class: the same answer from both, whatever it is.
            let other = if sclass == RENDER { KMS } else { RENDER };
            let (c, r) = scen::native_cmd(&d, other, e.cmd);
            assert_eq!(c, r, "{version}: {:#x} in class {other}", e.cmd);
            seen += 1;
        }
        assert!(seen > 20, "{version}: {seen} DRM entries");
        assert_eq!(scen::native_cmd(&d, MODESET, 0xc010_6d00), (0, 0));
        assert_eq!(scen::native_cmd(&d, RENDER, ioc(3, b'd', 0x3f, 8)), (0, 0));
        assert_eq!(scen::native_cmd(&d, KMS, ioc(3, b'Z', 0xa0, 64)), (0, 0));
    }
}

/// SYNCOBJ_HANDLE_TO_FD with the 16-byte struct, and with a 32-byte one,
/// handed to the interpreter as they are: both implementations refuse them
/// the same way (-EINVAL, a line in the log), send nothing and leave the
/// caller's memory alone. The native command both would normalise them to is
/// the 24-byte one.
#[test]
fn the_interpreter_takes_only_the_native_size() {
    for size in [16u32, 32] {
        let cmd = ioc(3, b'd', 0xc1, size);
        let mut w = world();
        let arg = vec![0x5au8; size as usize];
        w.mem.insert(ARG, arg.clone());
        let s = Scenario {
            seed: 0,
            dev: dev("610.57.04"),
            world: w,
            call: Call::I2 {
                sclass: RENDER,
                cmd,
                uarg: ARG,
                render: 5,
                xflags: 0,
            },
        };
        let o = scen::diff(&s).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(o.ret, -22, "size {size}");
        assert!(
            !o.world.events.iter().any(|e| matches!(e, Ev::Send(_))),
            "size {size}"
        );
        assert!(o.world.events.iter().any(|e| matches!(e, Ev::Warn(_))));
        assert_eq!(o.world.mem[&ARG], arg, "size {size}");
        assert_eq!(
            scen::native_cmd(&dev("610.57.04"), RENDER, cmd),
            (0xc018_64c1, 0xc018_64c1)
        );
    }
}
