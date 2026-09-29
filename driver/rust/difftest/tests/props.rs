// SPDX-License-Identifier: GPL-2.0-only
//! Properties of the Rust core on arbitrary input (proptest), and the
//! differential test on seeds proptest chooses and shrinks.

use nvgpu_guest_core::guest::deep::{self, Control, Count, Ptr};
use nvgpu_guest_core::guest::osdesc::{self, Described};
use nvgpu_guest_core::guest::schema;
use nvgpu_guest_core::guest::wire::{DEEP_SEGS_MAX, DEEP_SEGS_MAX_BYTES, OSDESC_MAX_PAGES};
use nvgpu_guest_difftest::scen;
use proptest::prelude::*;

fn ptr_strategy() -> impl Strategy<Value = Ptr> {
    (
        0u16..300,
        0u8..4,
        0u8..4,
        prop::array::uniform2((0u16..300, 0u8..9)),
        0u32..3,
        0u32..9,
    )
        .prop_map(|(ptr, flags, ncounts, c, scale, elem)| Ptr {
            ptr,
            flags,
            ncounts,
            counts: [
                Count {
                    offset: c[0].0,
                    width: c[0].1,
                },
                Count {
                    offset: c[1].0,
                    width: c[1].1,
                },
            ],
            scale,
            elem,
        })
}

struct NoMem;

impl deep::UserMem for NoMem {
    fn copy_from_user(&mut self, dst: &mut [u8], _: u64) -> Result<(), i32> {
        dst.fill(0x33);
        Ok(())
    }
    fn copy_to_user(&mut self, _: u64, _: &[u8]) -> Result<(), i32> {
        Ok(())
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// A deep plan never exceeds its budgets, and lays out exactly what it
    /// says it does.
    #[test]
    fn deep_plans_stay_in_bounds(
        ptrs in prop::array::uniform4(ptr_strategy()),
        nptrs in 0u32..6,
        blk in prop::collection::vec(any::<u8>(), 0..300),
    ) {
        let p = deep::plan(&Control { cmd: 0, nptrs, ptrs }, &blk);
        prop_assert!(p.n as usize <= DEEP_SEGS_MAX);
        let total: u64 = p.segs().iter().map(|s| u64::from(s.len)).sum();
        prop_assert!(total <= u64::from(DEEP_SEGS_MAX_BYTES));
        if p.n > 0 {
            prop_assert_eq!(u64::from(p.bytes), 8 + 8 * u64::from(p.n) + total);
            prop_assert!(p.segs().iter().all(|s| s.uptr != 0 && s.len != 0));
            let mut out = vec![0u8; p.bytes as usize];
            prop_assert!(deep::fill(&p, &mut NoMem, &mut out).is_ok());
            prop_assert!(deep::copy_back(&p, &mut NoMem, &out).is_ok());
        } else {
            prop_assert_eq!(p.bytes, 0);
        }
    }

    /// A registration is a range of at most the page budget, never zero
    /// pages, never wrapping.
    #[test]
    fn registrations_are_bounded(nr in prop::sample::select(vec![0x27u32, 0x2b, 0x4a, 0x2a]),
                                 len in prop::sample::select(vec![48usize, 56, 184, 60]),
                                 mut bytes in prop::collection::vec(any::<u8>(), 184),
                                 small in any::<bool>()) {
        // The word that makes it one of the three, and a descriptor type
        // and limit that make it plausible.
        let (at, want) = if nr == 0x4a { (8, 27u32) } else { (12, 0x71) };
        bytes[at..at + 4].copy_from_slice(&want.to_le_bytes());
        bytes[80..84].fill(0);
        if small {
            bytes[36..40].fill(0);
            bytes[76..80].fill(0);
        }
        if let Described::Ours(c) = osdesc::describe(&mut NoMem, nr, &bytes[..len.min(bytes.len())]) {
            let (off, n) = c.pages();
            prop_assert!(c.size >= 1 && c.va.checked_add(c.size).is_some());
            prop_assert!((1..=OSDESC_MAX_PAGES).contains(&n) && off < 4096);
            prop_assert!((off + c.size).div_ceil(4096) == n);
        }
    }

    /// Runs cover every page, in order.
    #[test]
    fn runs_cover_the_pages(pas in prop::collection::vec(0u64..64, 1..200)) {
        let pas: Vec<u64> = pas.iter().map(|p| p * 4096).collect();
        let mut got = Vec::new();
        let n = osdesc::runs(pas.len() as u64, &mut |i| pas[i as usize], &mut |g, l| got.push((g, l)));
        prop_assert_eq!(n, Some(got.len() as u64));
        let mut flat = Vec::new();
        for (g, l) in got {
            for i in 0..l {
                flat.push(g + i * 4096);
            }
        }
        prop_assert_eq!(flat, pas);
    }
}

/// The descriptor-kind test the KMS and NVKMS hooks make
/// (`nvgpu_fd_kind_allowed()`) and its Rust twin, over every device type
/// and every single kind bit, none and all.
#[test]
fn fd_kinds_agree_everywhere() {
    for t in 0u32..1100 {
        for kinds in (0..32).map(|b| 1u32 << b).chain([0, u32::MAX]) {
            let c = unsafe { nvgpu_guest_difftest::cabi::nvgpu_fd_kind_allowed(t, kinds) };
            assert_eq!(
                c,
                schema::fd_kind_allowed(t, kinds),
                "type {t} kinds {kinds:#x}"
            );
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    /// ... and on any pair.
    #[test]
    fn fd_kinds_agree_on_any_pair(t in any::<u32>(), kinds in any::<u32>()) {
        let c = unsafe { nvgpu_guest_difftest::cabi::nvgpu_fd_kind_allowed(t, kinds) };
        prop_assert_eq!(c, schema::fd_kind_allowed(t, kinds));
    }

    #[test]
    fn rm_escapes_agree_on_any_seed(seed in any::<u64>()) {
        let s = scen::gen_rm(seed);
        if let Err(e) = scen::diff(&s) {
            prop_assert!(false, "{}", e);
        }
    }

    #[test]
    fn atomic_agrees_on_any_seed(seed in any::<u64>()) {
        let s = scen::gen_atomic(seed);
        if let Err(e) = scen::diff(&s) {
            prop_assert!(false, "{}", e);
        }
    }

    #[test]
    fn ioctl2_agrees_on_any_seed(seed in any::<u64>()) {
        let s = scen::gen_i2(seed);
        if let Err(e) = scen::diff(&s) {
            prop_assert!(false, "{}", e);
        }
    }
}
