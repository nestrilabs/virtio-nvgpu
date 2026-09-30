// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use super::*;
use crate::nvos::NV_ERR_NOT_SUPPORTED;

fn all() -> Groups {
    let mut g = Groups::default();
    for x in Group::ALL {
        g.insert(x);
    }
    g
}

fn only(g: Group) -> Groups {
    let mut out = Groups::default();
    out.insert(g);
    out
}

fn yes(_: u32) -> bool {
    true
}

fn no(_: u32) -> bool {
    false
}

fn ctx<'a>(callers: &'a dyn Fn(u32) -> bool) -> Ctx<'a> {
    Ctx {
        callers,
        own: 0xc1d0_0001,
        deep_segments: false,
        deep_single: false,
        compute: true,
    }
}

fn put(p: &mut [u8], at: usize, v: u32) {
    p[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Every member of every group, in every release measured, is one this
/// module has a rule for, in the same group and at the size the release
/// measured; and every rule here is some release's member.
#[test]
fn every_member_has_a_rule_at_its_measured_size() {
    let mut used = std::collections::HashSet::new();
    for r in abi::rmallow::RELEASES {
        for g in r.groups {
            let group: Group = g.name.parse().expect("a group this module knows");
            for c in g.controls {
                let &(_, mg, size, _) = member(c.cmd)
                    .unwrap_or_else(|| panic!("{}: {:#x} has no rule here", r.version(), c.cmd));
                assert_eq!(mg, group, "{:#x}", c.cmd);
                assert_eq!(c.size, Some(size as u32), "{}: {:#x}", r.version(), c.cmd);
                used.insert(c.cmd);
            }
            for &k in g.classes {
                assert_eq!(group, Group::Profiling, "{k:#x}");
                assert!(matches!(
                    k,
                    MAXWELL_PROFILER_CONTEXT | MAXWELL_PROFILER_DEVICE
                ));
            }
        }
    }
    for m in MEMBERS {
        assert!(used.contains(&m.0), "{:#x}: in no release's group", m.0);
    }
    let names: Vec<&str> = Group::ALL.iter().map(|g| g.name()).collect();
    assert_eq!(names.len(), abi::rmallow::GROUP_NAMES.len());
    for n in abi::rmallow::GROUP_NAMES {
        assert!(names.contains(n), "{n}");
    }
}

/// With no group named -- the default -- no member is let through by the
/// allowlist, on any release, and the gate here has nothing to say.
#[test]
fn off_by_default_every_member_stays_refused() {
    use crate::rmallow::{Refusal, RmAllow};
    for r in abi::rmallow::RELEASES {
        let mut gate = RmAllow::default();
        gate.set_driver(r.version());
        for m in MEMBERS {
            let mut b = vec![0u8; 32];
            put(&mut b, 8, m.0);
            put(&mut b, 24, m.2 as u32);
            b.resize(32 + m.2, 0);
            assert_eq!(
                gate.check(abi::ioctl::NV_ESC_RM_CONTROL, &b, 32),
                Err(Refusal::Status {
                    at: 28,
                    status: NV_ERR_NOT_SUPPORTED
                }),
                "{}: {:#x}",
                r.version(),
                m.0
            );
            let p = vec![0u8; m.2];
            assert_eq!(
                control(Groups::default(), m.0, &p, m.2 as u32, &ctx(&yes)),
                Ok(None)
            );
        }
        for class in [MAXWELL_PROFILER_CONTEXT, MAXWELL_PROFILER_DEVICE] {
            let mut b = vec![0u8; 48];
            put(&mut b, 12, class);
            assert!(gate.check(abi::ioctl::NV_ESC_RM_ALLOC, &b, 48).is_err());
            assert_eq!(
                alloc(Groups::default(), class, &[0u8; 8], &ctx(&yes)),
                Ok(false)
            );
        }
    }
}

/// Named, a group's members are on the list of each release that has them
/// (and only those), and the others' are not.
#[test]
fn a_named_group_adds_its_members_and_no_others() {
    use crate::rmallow::RmAllow;
    let mut gate = RmAllow::default();
    gate.set_driver(abi::version::DriverVersion::new(610, 57, 4));
    gate.set_groups(only(Group::Thermal));
    let call = |gate: &mut RmAllow, cmd: u32, size: usize| {
        let mut b = vec![0u8; 32];
        put(&mut b, 8, cmd);
        put(&mut b, 24, size as u32);
        b.resize(32 + size, 0);
        gate.check(abi::ioctl::NV_ESC_RM_CONTROL, &b, 32)
    };
    assert_eq!(call(&mut gate, THERMAL_SYSTEM_EXECUTE_V2, 1432), Ok(()));
    assert!(call(&mut gate, THERMAL_SYSTEM_EXECUTE_V2, 1400).is_err());
    assert!(call(&mut gate, QUERY_ECC_CONFIGURATION, 8).is_err());
    assert!(call(&mut gate, MEMACCT_GET_LIMITS, 1032).is_err());
    gate.set_groups(all());
    assert_eq!(call(&mut gate, QUERY_ECC_CONFIGURATION, 8), Ok(()));
    assert_eq!(call(&mut gate, MEMACCT_GET_LIMITS, 1032), Ok(()));
    // GET_IMPL is 615's: not on 610's list, whatever is named.
    assert!(call(&mut gate, MEMACCT_GET_IMPL, 4).is_err());
    let mut b = vec![0u8; 48];
    put(&mut b, 12, MAXWELL_PROFILER_DEVICE);
    assert_eq!(gate.check(abi::ioctl::NV_ESC_RM_ALLOC, &b, 48), Ok(()));
    // Never a member: SET_LIMITS, the legacy profiler, the event buffer.
    assert!(call(&mut gate, 0x3d0d, 24).is_err());
    for class in [0x90cc, 0x90cd] {
        put(&mut b, 12, class);
        assert!(gate.check(abi::ioctl::NV_ESC_RM_ALLOC, &b, 48).is_err());
    }
    // Nor is a member carried by DEFERRED_API.
    let mut bundle = vec![0u8; 64];
    put(&mut bundle, 4, THERMAL_SYSTEM_EXECUTE_V2);
    for d in crate::nvos::DEFERRED_API_CONTROLS {
        let mut b = vec![0u8; 32];
        put(&mut b, 8, d);
        put(&mut b, 24, 64);
        b.extend_from_slice(&bundle);
        assert!(gate.check(abi::ioctl::NV_ESC_RM_CONTROL, &b, 32).is_err());
    }
}

#[test]
fn groups_parse_by_name_and_say_what_needs_compute() {
    let g = Groups::parse(["thermal,health", "debug"]).unwrap();
    assert!(g.contains(Group::Thermal) && g.contains(Group::Health) && g.contains(Group::Debug));
    assert!(!g.contains(Group::Profiling) && !g.contains(Group::Memacct));
    assert_eq!(g.to_string(), "thermal,health,debug");
    assert_eq!(g.needing_compute(), Some(Group::Debug));
    assert_eq!(Groups::parse(["thermal"]).unwrap().needing_compute(), None);
    assert!(Groups::parse(["thermal,bogus"]).is_err());
    assert!(Groups::parse(["all"]).is_err(), "no wildcard");
    assert_eq!(Groups::default().to_string(), "none");
    assert!(Groups::parse(Vec::<&str>::new()).unwrap().is_empty());
}

fn thermal_params(ops: &[u32]) -> Vec<u8> {
    let mut p = vec![0u8; 1432];
    put(&mut p, 0, 1);
    put(&mut p, 4, 0);
    put(&mut p, 8, 44);
    put(&mut p, 20, ops.len() as u32);
    for (i, &op) in ops.iter().enumerate() {
        put(&mut p, 24 + 44 * i + 8, op);
    }
    p
}

#[test]
fn thermal_lets_only_the_read_opcodes_through() {
    let g = only(Group::Thermal);
    let c = ctx(&no);
    let ok = |p: &[u8]| control(g, THERMAL_SYSTEM_EXECUTE_V2, p, 1432, &c);
    assert_eq!(
        ok(&thermal_params(&THERMAL_READ_OPCODES)),
        Ok(Some(Plan::default()))
    );
    assert_eq!(ok(&thermal_params(&[])), Ok(Some(Plan::default())));
    assert_eq!(
        ok(&thermal_params(&[0x1500; 32])),
        Ok(Some(Plan::default()))
    );
    // Any opcode the header does not name, anywhere in the list.
    for bad in [0, 0x102, 0x1501, 0x2000, 0x8000_1500, u32::MAX] {
        let r = ok(&thermal_params(&[0x1500, bad]));
        assert_eq!(
            r.map_err(|e| e.status),
            Err(NV_ERR_INSUFFICIENT_PERMISSIONS),
            "{bad:#x}"
        );
    }
    // An opcode past the count is not an instruction.
    let mut p = thermal_params(&[0x1500]);
    put(&mut p, 24 + 44 + 8, 0xdead);
    assert!(ok(&p).is_ok());
    // More than fit; another version, revision or instruction size, which
    // RM would hand GSP-RM whole; an unknown flag.
    for (at, v) in [(20, 33), (0, 2), (4, 1), (8, 48), (12, 2)] {
        let mut p = thermal_params(&[0x1500]);
        put(&mut p, at, v);
        assert_eq!(
            ok(&p).map_err(|e| e.status),
            Err(NV_ERR_INVALID_ARGUMENT),
            "{at}={v}"
        );
    }
    // RM's size, on any host.
    let p = thermal_params(&[0x1500]);
    assert_eq!(
        control(g, THERMAL_SYSTEM_EXECUTE_V2, &p, 1428, &c).map_err(|e| e.status),
        Err(NV_ERR_INVALID_PARAM_STRUCT)
    );
    assert_eq!(
        control(g, THERMAL_SYSTEM_EXECUTE_V2, &p[..1000], 1432, &c).map_err(|e| e.status),
        Err(NV_ERR_INVALID_PARAM_STRUCT)
    );
}

#[test]
fn health_is_its_size_and_nothing_else() {
    let g = only(Group::Health);
    for (cmd, size) in [
        (QUERY_ECC_CONFIGURATION, 8),
        (FB_GET_OFFLINED_PAGES, 2056),
        (BBX_GET_LAST_FLUSH_TIME, 16),
        (QUERY_INFOROM_ECC_SUPPORT, 0),
    ] {
        let p = vec![0xa5u8; size];
        assert_eq!(
            control(g, cmd, &p, size as u32, &ctx(&no)),
            Ok(Some(Plan::default()))
        );
        assert!(control(g, cmd, &p, size as u32 + 8, &ctx(&no)).is_err());
    }
}

#[test]
fn memacct_always_names_the_calling_process() {
    let g = only(Group::Memacct);
    for fd in [3u32, 0, 0xffff_fffe, u32::MAX] {
        let mut p = vec![0u8; 1032];
        put(&mut p, 0, fd);
        let plan = control(g, MEMACCT_GET_LIMITS, &p, 1032, &ctx(&no))
            .unwrap()
            .unwrap();
        assert_eq!(
            plan.fix,
            Some(Fix {
                at: 0,
                value: u32::MAX
            }),
            "{fd:#x}"
        );
        // What RM gets, and what the caller reads back.
        let mut sent = vec![0u8; 32];
        sent.extend_from_slice(&p);
        let to_rm = apply(&sent, plan.fix.unwrap());
        assert_eq!(le::i32_at(&to_rm, 32), Some(-1));
        let mut reply = to_rm.clone();
        restore(&mut reply, plan.fix.unwrap(), &sent);
        assert_eq!(le::u32_at(&reply, 32), Some(fd));
    }
    assert_eq!(
        control(g, MEMACCT_GET_IMPL, &[0u8; 4], 4, &ctx(&no)),
        Ok(Some(Plan::default()))
    );
}

fn memory_params(h: u32, length: u32, buffer: u64) -> Vec<u8> {
    let mut p = vec![0u8; 24];
    put(&mut p, 0, h);
    put(&mut p, 4, length);
    p[16..24].copy_from_slice(&buffer.to_le_bytes());
    p
}

#[test]
fn debug_memory_is_capped_segmented_the_callers_and_its_class_asked() {
    let g = only(Group::Debug);
    let seg = Ctx {
        deep_segments: true,
        ..ctx(&yes)
    };
    for (cmd, write) in [(DEBUG_READ_MEMORY, false), (DEBUG_WRITE_MEMORY, true)] {
        let p = memory_params(0x55, 4096, 0x7000);
        assert_eq!(
            control(g, cmd, &p, 24, &seg),
            Ok(Some(Plan {
                fix: None,
                memory: vec![(0x55, write)]
            }))
        );
        let status = |p: &[u8], c: &Ctx<'_>| control(g, cmd, p, 24, c).map_err(|e| e.status);
        // Up to the cap and not past it.
        let at_cap = memory_params(0x55, DEBUG_MEMORY_MAX, 0x7000);
        assert!(status(&at_cap, &seg).is_ok());
        let past = memory_params(0x55, DEBUG_MEMORY_MAX + 1, 0x7000);
        assert_eq!(status(&past, &seg), Err(NV_ERR_INVALID_ARGUMENT));
        let huge = memory_params(0x55, u32::MAX, 0x7000);
        assert_eq!(status(&huge, &seg), Err(NV_ERR_INVALID_ARGUMENT));
        // No length, no buffer.
        assert_eq!(
            status(&memory_params(0x55, 0, 0x7000), &seg),
            Err(NV_ERR_INVALID_ARGUMENT)
        );
        assert_eq!(
            status(&memory_params(0x55, 16, 0), &seg),
            Err(NV_ERR_INVALID_ARGUMENT)
        );
        // The buffer as a deep segment, sized by the table, and nothing else.
        assert_eq!(status(&p, &ctx(&yes)), Err(NV_ERR_INVALID_ARGUMENT));
        let single = Ctx {
            deep_single: true,
            ..ctx(&yes)
        };
        assert_eq!(status(&p, &single), Err(NV_ERR_INVALID_ARGUMENT));
        // Another process's client.
        let other = Ctx {
            deep_segments: true,
            ..ctx(&no)
        };
        assert_eq!(status(&p, &other), Err(NV_ERR_INSUFFICIENT_PERMISSIONS));
        // Without --allow-compute.
        let plain = Ctx {
            compute: false,
            ..seg
        };
        assert_eq!(status(&p, &plain), Err(NV_ERR_INSUFFICIENT_PERMISSIONS));
    }
    // The memory itself: system and video memory the client allocated;
    // registered pages only to read; never the register aperture.
    for (class, read, write) in [
        (0x3e, true, true),
        (0x40, true, true),
        (0x71, true, false),
        (0x3f, false, false),
        (0x50a0, false, false),
        (0xb1, false, false),
        (0x80, false, false),
    ] {
        assert_eq!(memory_class_ok(class, false).is_ok(), read, "{class:#x}");
        assert_eq!(memory_class_ok(class, true).is_ok(), write, "{class:#x}");
    }
    // The debug modes: set or clear, nothing else.
    for (cmd, ok) in [
        (DEBUG_SET_MODE_MMU_DEBUG, [1u32, 2].as_slice()),
        (DEBUG_SET_MODE_ERRBAR_DEBUG, [0u32, 1].as_slice()),
    ] {
        for a in 0..5u32 {
            let r = control(g, cmd, &a.to_le_bytes(), 4, &ctx(&yes));
            assert_eq!(r.is_ok(), ok.contains(&a), "{cmd:#x} {a}");
        }
        assert!(control(g, cmd, &1u32.to_le_bytes(), 4, &ctx(&no)).is_err());
    }
}

fn regops(ops: &[(u8, u8)]) -> Vec<u8> {
    let mut p = vec![0u8; 3980];
    put(&mut p, 0, ops.len() as u32);
    for (i, &(op, ty)) in ops.iter().enumerate() {
        p[12 + 32 * i] = op;
        p[12 + 32 * i + 1] = ty;
    }
    p
}

#[test]
fn profiling_is_context_switched_and_reads_its_contexts_registers_only() {
    let g = only(Group::Profiling);
    let c = ctx(&yes);
    let st = |cmd: u32, p: &[u8]| control(g, cmd, p, p.len() as u32, &c).map_err(|e| e.status);
    // Reservations: context-switched only.
    for cmd in [
        PROF_RESERVE_HWPM_LEGACY,
        PROF_RESERVE_PM_AREA_SMPC,
        PROF_RESERVE_CCU_PROF,
    ] {
        assert!(st(cmd, &[1]).is_ok(), "{cmd:#x}");
        for v in [0u8, 2, 0xff] {
            assert_eq!(
                st(cmd, &[v]),
                Err(NV_ERR_INSUFFICIENT_PERMISSIONS),
                "{cmd:#x}"
            );
        }
    }
    // A PMA stream: context-switched, into memory whose class RM is asked.
    let mut pma = vec![0u8; 56];
    put(&mut pma, 0, 0x55);
    put(&mut pma, 24, 0x66);
    assert_eq!(
        st(PROF_ALLOC_PMA_STREAM, &pma),
        Err(NV_ERR_INSUFFICIENT_PERMISSIONS)
    );
    pma[40] = 1;
    assert_eq!(
        control(g, PROF_ALLOC_PMA_STREAM, &pma, 56, &c),
        Ok(Some(Plan {
            fix: None,
            memory: vec![(0x55, true), (0x66, true)]
        }))
    );
    // No wait inside RM.
    let mut upd = vec![0u8; 48];
    assert!(st(PROF_PMA_STREAM_UPDATE_GET_PUT, &upd).is_ok());
    upd[9] = 1;
    assert_eq!(
        st(PROF_PMA_STREAM_UPDATE_GET_PUT, &upd),
        Err(NV_ERR_INVALID_ARGUMENT)
    );
    // Register operations: reads of the context's image.
    let ctx_types = [0x01u8, 0x02, 0x04, 0x08, 0x10, 0x40];
    for op in [0u8, 2, 4] {
        for ty in ctx_types {
            assert!(
                st(PROF_EXEC_REG_OPS, &regops(&[(op, ty)])).is_ok(),
                "{op} {ty:#x}"
            );
        }
    }
    assert!(st(PROF_EXEC_REG_OPS, &regops(&[(0, 1); 124])).is_ok());
    for (op, ty) in [
        (1u8, 1u8),
        (3, 1),
        (5, 1),
        (6, 1),
        (0, 0),
        (0, 0x20),
        (0, 0x80),
        (2, 0x03),
    ] {
        assert_eq!(
            st(PROF_EXEC_REG_OPS, &regops(&[(0, 1), (op, ty)])),
            Err(NV_ERR_INSUFFICIENT_PERMISSIONS),
            "{op} {ty:#x}"
        );
    }
    let mut none = regops(&[]);
    assert_eq!(st(PROF_EXEC_REG_OPS, &none), Err(NV_ERR_INVALID_ARGUMENT));
    put(&mut none, 0, 125);
    assert_eq!(st(PROF_EXEC_REG_OPS, &none), Err(NV_ERR_INVALID_ARGUMENT));
    let mut mode = regops(&[(0, 1)]);
    put(&mut mode, 4, 2);
    assert_eq!(st(PROF_EXEC_REG_OPS, &mode), Err(NV_ERR_INVALID_ARGUMENT));
    // Every profiling call from the process that made its client.
    assert!(control(g, PROF_RESERVE_HWPM_LEGACY, &[1], 1, &ctx(&no)).is_err());
    assert!(control(g, PROF_RELEASE_HWPM_LEGACY, &[], 0, &ctx(&no)).is_err());
}

#[test]
fn a_profiler_binds_a_context_of_the_callers_or_is_refused() {
    let g = only(Group::Profiling);
    let own = 0xc1d0_0001;
    let mine = |h: u32| h == own || h == 0xc1d0_0002;
    let c = Ctx {
        callers: &mine,
        own,
        deep_segments: false,
        deep_single: false,
        compute: true,
    };
    let b2cc = |client: u32, context: u32| {
        let mut p = vec![0u8; 8];
        put(&mut p, 0, client);
        put(&mut p, 4, context);
        alloc(g, MAXWELL_PROFILER_DEVICE, &p, &c).map_err(|e| e.status)
    };
    assert_eq!(b2cc(own, 0xc4a), Ok(true));
    assert_eq!(b2cc(0xc1d0_0002, 0xc4a), Ok(true));
    // Device-wide: no context to bind to.
    assert_eq!(b2cc(0, 0), Err(NV_ERR_INSUFFICIENT_PERMISSIONS));
    assert_eq!(b2cc(own, 0), Err(NV_ERR_INSUFFICIENT_PERMISSIONS));
    // Another process's context.
    assert_eq!(
        b2cc(0xc1d0_0003, 0xc4a),
        Err(NV_ERR_INSUFFICIENT_PERMISSIONS)
    );
    assert_eq!(
        alloc(g, MAXWELL_PROFILER_DEVICE, &[0u8; 4], &c).map_err(|e| e.status),
        Err(NV_ERR_INVALID_ARGUMENT)
    );
    assert_eq!(alloc(g, MAXWELL_PROFILER_CONTEXT, &[0u8; 4], &c), Ok(true));
    // Through a client of another process's, or without compute.
    let theirs = Ctx {
        own: 0xc1d0_0003,
        ..c
    };
    assert!(alloc(g, MAXWELL_PROFILER_CONTEXT, &[0u8; 4], &theirs).is_err());
    let plain = Ctx {
        compute: false,
        ..c
    };
    assert!(alloc(g, MAXWELL_PROFILER_CONTEXT, &[0u8; 4], &plain).is_err());
    // Other classes are not this gate's.
    assert_eq!(alloc(g, 0x90cc, &[], &c), Ok(false));
}

// ────────────────────────── through the backend ──────────────────────────

mod backend {
    use super::*;
    use crate::hostfd::{HandleKind, IOC_RW, ioc};
    use crate::le::u32_at as rd32;
    use crate::testing::rm;
    use abi::version::DriverVersion;
    use protocol::messages::{
        DEEP_SEGMENTED, DeviceKind, GCAP_PROC_EUID, GCAP_PROC_ID, HELLO_F_FRESH, MsgType, PROTO_V2,
        ProcId,
    };
    use std::os::fd::OwnedFd;

    const ALLOC: u32 = ioc(IOC_RW, b'F', 0x2b, 48);
    const CONTROL: u32 = ioc(IOC_RW, b'F', 0x2a, 32);
    /// Where a v1 reply's parameters start: MsgHeader, IoctlResp.
    const BODY: usize = 16 + 12;
    const DEVICE: u32 = 0xde7;
    const SUBDEVICE: u32 = 0x2080;
    const DEBUGGER: u32 = 0x83de;
    const SYSMEM: u32 = 0x55;
    const VIDMEM: u32 = 0x56;
    const REGMEM: u32 = 0x57;
    const OSDESC: u32 = 0x58;

    /// A v2 session whose guest names its processes, the host release
    /// `v`, `groups` served, compute on, and one control file.
    fn vm(v: DriverVersion, groups: Groups) -> (NvidiaBackend, u32) {
        rm::seen();
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        rm::install(&mut be);
        let mut req = Vec::new();
        for w in [MsgType::Hello as u32, 0, 0, 1] {
            req.extend_from_slice(&w.to_le_bytes());
        }
        for w in [PROTO_V2, HELLO_F_FRESH, GCAP_PROC_ID | GCAP_PROC_EUID, 0] {
            req.extend_from_slice(&w.to_le_bytes());
        }
        let mut resp = vec![0u8; 256];
        assert!(be.dispatch(&req, &mut resp) >= 24);
        be.config_mut().allow_compute = true;
        be.rmallow.set_driver(v);
        be.set_rm_groups(groups);
        let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let f = be.adopt_for_test(null, HandleKind::Dev(DeviceKind::Ctl));
        (be, f)
    }

    fn pid(tgid: u32) -> ProcId {
        ProcId {
            start_ns: 1_000_000 + u64::from(tgid),
            tgid,
            euid: 1000 + tgid,
        }
    }

    /// A v1 IOCTL: `deep` a segment table's segments, each (pointer
    /// offset, bytes), sent after the parameters; the caller after that.
    fn call(
        be: &mut NvidiaBackend,
        handle: u32,
        cmd: u32,
        outer: &[u8],
        nested: &[u8],
        deep: &[(u32, &[u8])],
        by: ProcId,
    ) -> Vec<u8> {
        let mut block = Vec::new();
        if !deep.is_empty() {
            block.extend_from_slice(&(deep.len() as u32).to_le_bytes());
            block.extend_from_slice(&0u32.to_le_bytes());
            for (off, b) in deep {
                block.extend_from_slice(&off.to_le_bytes());
                block.extend_from_slice(&(b.len() as u32).to_le_bytes());
            }
            for (_, b) in deep {
                block.extend_from_slice(b);
            }
        }
        let mut req = Vec::new();
        let nested_offset = if nested.is_empty() { 0 } else { outer.len() };
        let deep_at = if deep.is_empty() { 0 } else { DEEP_SEGMENTED };
        for v in [
            MsgType::Ioctl as u32,
            handle,
            0,
            0,
            cmd,
            outer.len() as u32,
            nested_offset as u32,
            nested.len() as u32,
            deep_at,
            block.len() as u32,
        ] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(outer);
        req.extend_from_slice(nested);
        req.extend_from_slice(&block);
        req.extend_from_slice(&by.start_ns.to_le_bytes());
        req.extend_from_slice(&by.tgid.to_le_bytes());
        req.extend_from_slice(&by.euid.to_le_bytes());
        let mut resp = vec![0u8; 1 << 20];
        let n = be.dispatch(&req, &mut resp);
        resp.truncate(n);
        resp
    }

    fn status(r: &[u8], at: usize) -> u32 {
        assert_eq!(rd32(r, 8), Some(0), "the ioctl succeeds; RM's status says");
        rd32(r, BODY + at).unwrap()
    }

    fn words(ws: &[(usize, u32)], len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
        for &(at, v) in ws {
            put(&mut b, at, v);
        }
        b
    }

    /// A client made by process `tgid` on `f`, with a device, subdevice,
    /// debugger and memory of four kinds made in RM directly.
    fn client(be: &mut NvidiaBackend, f: u32, tgid: u32) -> u32 {
        let r = call(be, f, ALLOC, &words(&[(12, 0x41)], 48), &[], &[], pid(tgid));
        assert_eq!(status(&r, 40), 0);
        let c = rd32(&r, BODY + 8).unwrap();
        rm::with(|rm| {
            rm.alloc(c, c, DEVICE, 0x80).unwrap();
            rm.alloc(c, DEVICE, SUBDEVICE, 0x2080).unwrap();
            rm.alloc(c, SUBDEVICE, DEBUGGER, 0x83de).unwrap();
            for (h, class) in [
                (SYSMEM, 0x3e),
                (VIDMEM, 0x40),
                (REGMEM, 0x3f),
                (OSDESC, 0x71),
            ] {
                rm.alloc(c, DEVICE, h, class).unwrap();
            }
        });
        c
    }

    fn control_block(client: u32, object: u32, cmd: u32, size: usize) -> Vec<u8> {
        words(&[(0, client), (4, object), (8, cmd), (24, size as u32)], 32)
    }

    fn reached(cmd: u32) -> bool {
        rm::seen()
            .iter()
            .any(|c| c.nr == abi::ioctl::NV_ESC_RM_CONTROL && c.key == cmd)
    }

    #[test]
    fn thermal_off_is_not_supported_and_on_reaches_rm_with_read_opcodes_only() {
        let v = DriverVersion::new(610, 57, 4);
        let (mut be, f) = vm(v, Groups::default());
        let c = client(&mut be, f, 10);
        let top = control_block(c, SUBDEVICE, THERMAL_SYSTEM_EXECUTE_V2, 1432);
        let good = thermal_params(&[0x1500]);
        rm::seen();
        let r = call(&mut be, f, CONTROL, &top, &good, &[], pid(10));
        assert_eq!(status(&r, 28), NV_ERR_NOT_SUPPORTED, "off by default");
        assert!(!reached(THERMAL_SYSTEM_EXECUTE_V2));

        let (mut be, f) = vm(v, only(Group::Thermal));
        let c = client(&mut be, f, 10);
        let top = control_block(c, SUBDEVICE, THERMAL_SYSTEM_EXECUTE_V2, 1432);
        rm::seen();
        let r = call(&mut be, f, CONTROL, &top, &good, &[], pid(10));
        assert_eq!(status(&r, 28), 0);
        assert!(reached(THERMAL_SYSTEM_EXECUTE_V2));
        let bad = thermal_params(&[0x1500, 0x2000]);
        let r = call(&mut be, f, CONTROL, &top, &bad, &[], pid(10));
        assert_eq!(status(&r, 28), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(THERMAL_SYSTEM_EXECUTE_V2), "RM not asked");
    }

    #[test]
    fn memacct_hands_rm_the_calling_process_and_the_caller_its_own_descriptor() {
        let v = DriverVersion::new(615, 71, 9);
        let (mut be, f) = vm(v, Groups::default());
        let c = client(&mut be, f, 10);
        let top = control_block(c, c, MEMACCT_GET_LIMITS, 1032);
        let p = words(&[(0, 7)], 1032);
        rm::seen();
        let r = call(&mut be, f, CONTROL, &top, &p, &[], pid(10));
        assert_eq!(
            status(&r, 28),
            NV_ERR_NOT_SUPPORTED,
            "off: rmctl.rs answers"
        );
        assert!(!reached(MEMACCT_GET_LIMITS));

        let (mut be, f) = vm(v, only(Group::Memacct));
        let c = client(&mut be, f, 10);
        let top = control_block(c, c, MEMACCT_GET_LIMITS, 1032);
        rm::seen();
        let r = call(&mut be, f, CONTROL, &top, &p, &[], pid(10));
        assert_eq!(status(&r, 28), 0);
        assert!(reached(MEMACCT_GET_LIMITS));
        let to_rm = rm::with(|rm| rm.last_params.clone());
        assert_eq!(le::i32_at(&to_rm, 0), Some(MEMACCT_CURRENT_PROCESS));
        assert_eq!(
            rd32(&r, BODY + 32),
            Some(7),
            "the caller reads its own back"
        );
        // SET_LIMITS stays refused, group or not.
        let top = control_block(c, c, 0x3d0d, 24);
        let r = call(&mut be, f, CONTROL, &top, &[0u8; 24], &[], pid(10));
        assert_eq!(status(&r, 28), NV_ERR_NOT_SUPPORTED);
        assert!(!reached(0x3d0d));
    }

    #[test]
    fn debug_reads_the_callers_memory_and_never_the_register_aperture() {
        let v = DriverVersion::new(595, 99, 2);
        let (mut be, f) = vm(v, only(Group::Debug));
        let c = client(&mut be, f, 10);
        let other = client(&mut be, f, 20);
        let top = control_block(c, DEBUGGER, DEBUG_READ_MEMORY, 24);
        let buf = vec![0u8; 256];
        let read = |be: &mut NvidiaBackend, top: &[u8], h: u32, by: u32| {
            let p = memory_params(h, 256, 0x7000_0000);
            call(be, f, CONTROL, top, &p, &[(16, &buf)], pid(by))
        };
        for h in [SYSMEM, VIDMEM, OSDESC] {
            rm::seen();
            let r = read(&mut be, &top, h, 10);
            assert_eq!(status(&r, 28), 0, "{h:#x}");
            assert!(reached(DEBUG_READ_MEMORY), "{h:#x}");
        }
        // The register aperture: RM is asked its class, and no more.
        rm::seen();
        let r = read(&mut be, &top, REGMEM, 10);
        assert_eq!(status(&r, 28), NV_ERR_INSUFFICIENT_PERMISSIONS);
        let s = rm::seen();
        assert!(s.iter().any(|x| x.key == GET_HANDLE_INFO));
        assert!(!s.iter().any(|x| x.key == DEBUG_READ_MEMORY));
        // A handle RM does not know: refused the same way.
        let r = read(&mut be, &top, 0x999, 10);
        assert_eq!(status(&r, 28), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(DEBUG_READ_MEMORY));
        // Through another process's client.
        let theirs = control_block(other, DEBUGGER, DEBUG_READ_MEMORY, 24);
        let r = read(&mut be, &theirs, SYSMEM, 10);
        assert_eq!(status(&r, 28), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(DEBUG_READ_MEMORY));
        // Registered pages are never written.
        let wtop = control_block(c, DEBUGGER, DEBUG_WRITE_MEMORY, 24);
        for (h, want) in [(SYSMEM, 0), (OSDESC, NV_ERR_INSUFFICIENT_PERMISSIONS)] {
            rm::seen();
            let p = memory_params(h, 256, 0x7000_0000);
            let r = call(&mut be, f, CONTROL, &wtop, &p, &[(16, &buf)], pid(10));
            assert_eq!(status(&r, 28), want, "{h:#x}");
            assert_eq!(reached(DEBUG_WRITE_MEMORY), want == 0, "{h:#x}");
        }
        // Past the cap: refused before its segment is looked at.
        let p = memory_params(SYSMEM, DEBUG_MEMORY_MAX + 1, 0x7000_0000);
        let r = call(&mut be, f, CONTROL, &top, &p, &[(16, &buf)], pid(10));
        assert_eq!(status(&r, 28), NV_ERR_INVALID_ARGUMENT);
        assert!(!reached(DEBUG_READ_MEMORY));
        // A segment of another length than `length`: deepseg.rs's refusal,
        // RM not asked.
        let p = memory_params(SYSMEM, 128, 0x7000_0000);
        let r = call(&mut be, f, CONTROL, &top, &p, &[(16, &buf)], pid(10));
        assert_ne!(rd32(&r, 8), Some(0), "the ioctl fails");
        assert!(!reached(DEBUG_READ_MEMORY));
    }

    #[test]
    fn a_device_wide_profiler_is_refused_and_a_context_one_reaches_rm() {
        let v = DriverVersion::new(595, 99, 2);
        let (mut be, f) = vm(v, only(Group::Profiling));
        let c = client(&mut be, f, 10);
        let alloc_b2cc = |be: &mut NvidiaBackend, target: u32, by: u32| {
            let outer = words(
                &[
                    (0, c),
                    (4, SUBDEVICE),
                    (8, 0xb2c0),
                    (12, MAXWELL_PROFILER_DEVICE),
                    (32, 8),
                ],
                48,
            );
            let p = words(&[(0, target), (4, 0xc4a)], 8);
            call(be, f, ALLOC, &outer, &p, &[], pid(by))
        };
        rm::seen();
        let r = alloc_b2cc(&mut be, 0, 10);
        assert_eq!(status(&r, 40), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(rm::seen().is_empty(), "RM not asked");
        let r = alloc_b2cc(&mut be, c, 10);
        assert_eq!(status(&r, 40), 0);
        // Made through the caller's client by another process.
        let r = alloc_b2cc(&mut be, c, 20);
        assert_eq!(status(&r, 40), NV_ERR_INSUFFICIENT_PERMISSIONS);
    }
}
