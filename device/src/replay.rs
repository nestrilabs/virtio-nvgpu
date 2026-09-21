//! Replaying real mapping lifetimes against the SHM allocator.
//!
//! `device/fixtures/mapping-replay.tsv` is the sequence of
//! `NV_ESC_RM_MAP_MEMORY` and `NV_ESC_RM_UNMAP_MEMORY` calls a Tesla T4 on
//! driver 580.178.04 actually issued, with the real lengths and caching types,
//! and each unmap paired to its map by `pLinearAddress`.
//!
//! The question these tests answer is the one that cannot be answered by
//! inspection: does the allocator survive a guest that runs more than one
//! workload? A guest does not map once and stop. It runs an encode, tears it
//! down, and runs another.

#[cfg(test)]
mod tests {
    use crate::shm::{PgprotKind, ShmAllocator, ShmRegion};
    use std::collections::HashMap;

    #[derive(Debug)]
    enum Ev {
        Map { id: u32, length: u64, zone: PgprotKind },
        Unmap { id: u32 },
    }

    struct Workload {
        name: String,
        events: Vec<Ev>,
    }

    const FIXTURE: &str = include_str!("../fixtures/mapping-replay.tsv");

    fn workloads() -> Vec<Workload> {
        let mut out: Vec<Workload> = Vec::new();
        for line in FIXTURE.lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let f: Vec<&str> = line.split('\t').collect();
            match f[0] {
                "W" => out.push(Workload { name: f[1].to_string(), events: Vec::new() }),
                "M" => {
                    let zone = match f[3] {
                        "uc" => PgprotKind::Uncached,
                        "wc" => PgprotKind::WriteCombine,
                        "wb" => PgprotKind::WriteBack,
                        other => panic!("unknown zone {other}"),
                    };
                    out.last_mut().expect("M before W").events.push(Ev::Map {
                        id: f[1].parse().expect("id"),
                        length: f[2].parse().expect("length"),
                        zone,
                    });
                }
                "U" => out.last_mut().expect("U before W").events.push(Ev::Unmap {
                    id: f[1].parse().expect("id"),
                }),
                other => panic!("unknown record {other}"),
            }
        }
        out
    }

    /// Run one workload's map/unmap sequence. Returns the regions still live at
    /// the end -- a process that exits without unmapping is the normal case.
    fn run(
        shm: &mut ShmAllocator,
        w: &Workload,
        iteration: usize,
    ) -> Vec<ShmRegion> {
        let mut live: HashMap<u32, ShmRegion> = HashMap::new();
        for ev in &w.events {
            match ev {
                Ev::Map { id, length, zone } => {
                    let region = shm.alloc(*length, *zone).unwrap_or_else(|e| {
                        let (uc, wc, wb) = shm.free_bytes();
                        panic!(
                            "{} iteration {}: mapping {} of {} bytes failed: {}\n  \
                             free: uc={} wc={} wb={}",
                            w.name, iteration, id, length, e, uc, wc, wb
                        )
                    });
                    live.insert(*id, region);
                }
                Ev::Unmap { id } => {
                    let region = live
                        .remove(id)
                        .unwrap_or_else(|| panic!("{}: unmap of unknown id {}", w.name, id));
                    shm.free(&region)
                        .unwrap_or_else(|e| panic!("{}: free of id {} failed: {}", w.name, id, e));
                }
            }
        }
        live.into_values().collect()
    }

    #[test]
    fn fixture_parses_into_three_workloads() {
        let w = workloads();
        assert_eq!(w.len(), 3, "expected vulkaninfo, cuda-kernel and nvenc-h264");
        assert!(w.iter().all(|w| !w.events.is_empty()));
    }

    /// The regression this whole change exists for.
    ///
    /// With the old bump allocator the second iteration of nvenc-h264 failed:
    /// one 3-second 1080p encode maps ~116 MiB into a 128 MiB write-combine
    /// zone, and nothing was ever returned.
    #[test]
    fn a_guest_can_run_each_workload_many_times() {
        for w in workloads() {
            let mut shm = ShmAllocator::with_default_zones();
            for i in 1..=25 {
                let leftover = run(&mut shm, &w, i);
                // Process exit: whatever is still mapped is reclaimed.
                for region in leftover {
                    shm.free(&region).expect("teardown free");
                }
            }
        }
    }

    /// Interleaved workloads, as a guest running a compositor and an encoder
    /// at once would produce.
    #[test]
    fn workloads_can_run_concurrently_and_repeatedly() {
        let ws = workloads();
        let mut shm = ShmAllocator::with_default_zones();
        for i in 1..=10 {
            let mut live = Vec::new();
            for w in &ws {
                live.extend(run(&mut shm, w, i));
            }
            for region in live {
                shm.free(&region).expect("teardown free");
            }
        }
    }

    /// After a full cycle the zones must be exactly as empty as they started,
    /// or something is leaking a little each time.
    #[test]
    fn a_completed_cycle_returns_every_byte() {
        let ws = workloads();
        let mut shm = ShmAllocator::with_default_zones();
        let before = shm.free_bytes();
        for w in &ws {
            let live = run(&mut shm, w, 1);
            for region in live {
                shm.free(&region).expect("teardown free");
            }
        }
        assert_eq!(before, shm.free_bytes(), "zones did not return to their initial free size");
    }

    /// Fragmentation check: repeated cycles must not whittle down the largest
    /// contiguous extent, or a big mapping eventually fails while plenty of
    /// total space remains.
    #[test]
    fn repeated_cycles_do_not_fragment_the_zones() {
        let ws = workloads();
        let mut shm = ShmAllocator::with_default_zones();
        let mut first = None;
        for i in 1..=15 {
            for w in &ws {
                let live = run(&mut shm, w, i);
                for region in live {
                    shm.free(&region).expect("teardown free");
                }
            }
            let largest = shm.largest_free();
            match first {
                None => first = Some(largest),
                Some(f) => assert_eq!(
                    f, largest,
                    "largest contiguous extent changed by cycle {i}: {f:?} -> {largest:?}"
                ),
            }
        }
    }

    /// A double free must be refused rather than corrupting the free list.
    #[test]
    fn freeing_twice_is_rejected() {
        let mut shm = ShmAllocator::with_default_zones();
        let region = shm.alloc(65536, PgprotKind::WriteCombine).expect("alloc");
        shm.free(&region).expect("first free");
        assert!(shm.free(&region).is_err(), "second free must be rejected");
    }

    /// Peak write-combine use per workload, measured rather than asserted from
    /// memory. These are the numbers `ZoneConfig::default_1gib` is sized
    /// against; if the fixture or the allocator changes, this fails and the
    /// zone sizing gets revisited instead of silently drifting.
    #[test]
    fn peak_zone_use_matches_what_the_t4_did() {
        let expect = [
            ("vulkaninfo", 15.9_f64),
            ("cuda-kernel", 67.7),
            ("nvenc-h264", 116.5),
        ];
        for w in workloads() {
            let mut shm = ShmAllocator::with_default_zones();
            let (_, wc_start, _) = shm.free_bytes();
            let mut live: HashMap<u32, ShmRegion> = HashMap::new();
            let mut peak = 0u64;
            for ev in &w.events {
                match ev {
                    Ev::Map { id, length, zone } => {
                        let r = shm.alloc(*length, *zone).expect("alloc");
                        live.insert(*id, r);
                        let (_, wc_now, _) = shm.free_bytes();
                        peak = peak.max(wc_start - wc_now);
                    }
                    Ev::Unmap { id } => {
                        let r = live.remove(id).expect("live region");
                        shm.free(&r).expect("free");
                    }
                }
            }
            let want = expect
                .iter()
                .find(|(n, _)| *n == w.name)
                .unwrap_or_else(|| panic!("no expected peak for {}", w.name))
                .1;
            let got = peak as f64 / (1024.0 * 1024.0);
            assert!(
                (got - want).abs() < 0.5,
                "{}: peak write-combine use {got:.2} MiB, expected about {want:.1} MiB",
                w.name
            );
        }
    }

    /// Three workloads at once is the shape of a real guest: a compositor
    /// rendering, CUDA holding its allocations and an encoder running. This
    /// overran the old 128 MiB write-combine zone.
    #[test]
    fn three_concurrent_workloads_fit() {
        let ws = workloads();
        let mut shm = ShmAllocator::with_default_zones();
        let (_, wc_start, _) = shm.free_bytes();
        let mut all_live = Vec::new();
        let mut peak = 0u64;
        for w in &ws {
            let mut live: HashMap<u32, ShmRegion> = HashMap::new();
            for ev in &w.events {
                match ev {
                    Ev::Map { id, length, zone } => {
                        let r = shm.alloc(*length, *zone).unwrap_or_else(|e| {
                            panic!("{} mapping {id} of {length} bytes failed: {e}", w.name)
                        });
                        live.insert(*id, r);
                        let (_, wc_now, _) = shm.free_bytes();
                        peak = peak.max(wc_start - wc_now);
                    }
                    Ev::Unmap { id } => {
                        let r = live.remove(id).expect("live region");
                        shm.free(&r).expect("free");
                    }
                }
            }
            all_live.extend(live.into_values());
        }
        let peak_mib = peak as f64 / (1024.0 * 1024.0);
        assert!(
            (peak_mib - 184.6).abs() < 1.0,
            "concurrent peak {peak_mib:.2} MiB, expected about 184.6 MiB"
        );
        for r in all_live {
            shm.free(&r).expect("teardown free");
        }
    }
}
