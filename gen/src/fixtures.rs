//! Checking generated tables against a real driver.
//!
//! `gen/fixtures/*.tsv` records the ioctl parameter sizes actually observed on
//! hardware, captured with `nvidia_sniffer` under `LD_PRELOAD`. The tables in
//! `versions/` are derived from gVisor's nvproxy; the fixtures are derived from
//! a running GPU. Neither is checked against the other anywhere else, so a
//! disagreement here means one of them is wrong about a driver we claim to
//! support -- which is exactly the failure that is otherwise found only at
//! runtime, in a guest, as a silently truncated ioctl.
//!
//! Two captures, chosen to disagree if anything is version- or
//! architecture-specific:
//!
//! | fixture | driver | GPU | profile it resolves to |
//! | --- | --- | --- | --- |
//! | `580.178.04.tsv` | 580.178.04 | Tesla T4 (Turing) | its own |
//! | `615.71.09.tsv` | 615.71.09 | RTX A2000 (Ampere) | 595.71.05, by range |
//!
//! The second is the one that earns its keep. 615.71.09 has no profile of its
//! own, so it exercises the range selection that the whole versioning scheme
//! rests on: if picking the highest profile `<=` the driver version were
//! wrong, this is where it would show.

#[cfg(test)]
mod tests {
    use crate::ioctl;
    use crate::version::DriverVersion;
    use crate::versions::{lookup, table_for, IoctlEntry, IoctlKind};

    /// `(name, observed_size, call_count)`; size `-1` means it varied.
    fn parse(tsv: &str) -> Vec<(&str, i64, u64)> {
        tsv.lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| {
                let mut f = l.split('\t');
                let name = f.next().expect("escape name");
                let size = f.next().expect("size").parse().expect("size is an integer");
                let count = f.next().expect("count").parse().expect("count is an integer");
                (name, size, count)
            })
            .collect()
    }

    /// Map the names used in the fixture to escape numbers.
    fn escape_of(name: &str) -> Option<u32> {
        use ioctl::*;
        Some(match name {
            "NV_ESC_CARD_INFO" => NV_ESC_CARD_INFO,
            "NV_ESC_REGISTER_FD" => NV_ESC_REGISTER_FD,
            "NV_ESC_ALLOC_OS_EVENT" => NV_ESC_ALLOC_OS_EVENT,
            "NV_ESC_FREE_OS_EVENT" => NV_ESC_FREE_OS_EVENT,
            "NV_ESC_CHECK_VERSION_STR" => NV_ESC_CHECK_VERSION_STR,
            "NV_ESC_ATTACH_GPUS_TO_FD" => NV_ESC_ATTACH_GPUS_TO_FD,
            "NV_ESC_SYS_PARAMS" => NV_ESC_SYS_PARAMS,
            "NV_ESC_NUMA_INFO" => NV_ESC_NUMA_INFO,
            "NV_ESC_WAIT_OPEN_COMPLETE" => NV_ESC_WAIT_OPEN_COMPLETE,
            "NV_ESC_RM_ALLOC_MEMORY" => NV_ESC_RM_ALLOC_MEMORY,
            "NV_ESC_RM_FREE" => NV_ESC_RM_FREE,
            "NV_ESC_RM_CONTROL" => NV_ESC_RM_CONTROL,
            "NV_ESC_RM_ALLOC" => NV_ESC_RM_ALLOC,
            "NV_ESC_RM_DUP_OBJECT" => NV_ESC_RM_DUP_OBJECT,
            "NV_ESC_RM_IDLE_CHANNELS" => NV_ESC_RM_IDLE_CHANNELS,
            "NV_ESC_RM_VID_HEAP_CONTROL" => NV_ESC_RM_VID_HEAP_CONTROL,
            "NV_ESC_RM_MAP_MEMORY" => NV_ESC_RM_MAP_MEMORY,
            "NV_ESC_RM_UNMAP_MEMORY" => NV_ESC_RM_UNMAP_MEMORY,
            "NV_ESC_RM_ALLOC_CONTEXT_DMA2" => NV_ESC_RM_ALLOC_CONTEXT_DMA2,
            "NV_ESC_RM_MAP_MEMORY_DMA" => NV_ESC_RM_MAP_MEMORY_DMA,
            "NV_ESC_RM_UNMAP_MEMORY_DMA" => NV_ESC_RM_UNMAP_MEMORY_DMA,
            "NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO" => NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO,
            _ => return None,
        })
    }

    const T4_580: &str = include_str!("../fixtures/580.178.04.tsv");
    const A2000_615: &str = include_str!("../fixtures/615.71.09.tsv");

    fn table_of(v: DriverVersion) -> &'static [IoctlEntry] {
        table_for(v).unwrap_or_else(|| panic!("{v} resolves to no table"))
    }

    /// Every size in `tsv` matches what the table selected for `version` says.
    /// `hw` names the machine, so a failure says which capture disagreed.
    fn sizes_match(tsv: &str, version: DriverVersion, hw: &str) {
        let table = table_of(version);
        let mut checked = 0;

        for (name, observed, _count) in parse(tsv) {
            let escape = escape_of(name).unwrap_or_else(|| {
                panic!("fixture names {name}, which gen/src/ioctl.rs does not define")
            });
            let entry = lookup(table, escape).unwrap_or_else(|| {
                panic!("{name} was observed on hardware but is absent from the {version} table")
            });

            match entry.kind {
                // Variable-length ioctls carry an array; the observed size is
                // however many elements that call happened to pass, so there is
                // nothing to compare against.
                IoctlKind::Bytes => assert!(
                    entry.param_size.is_none(),
                    "{name} is Bytes but claims a fixed size of {:?}",
                    entry.param_size
                ),
                _ => {
                    let expected = entry
                        .param_size
                        .unwrap_or_else(|| panic!("{name} has no size in the {version} table"));
                    assert_eq!(
                        expected as i64, observed,
                        "{name}: table says {expected} bytes, {hw} running {version} issued {observed}"
                    );
                    checked += 1;
                }
            }
        }

        // Guard against the fixture silently emptying out.
        assert!(checked >= 15, "only {checked} sizes were actually compared for {hw}");
    }

    /// Nothing the hardware used is missing from the table it resolves to.
    fn all_escapes_known(tsv: &str, version: DriverVersion) {
        let table = table_of(version);
        let missing: Vec<_> = parse(tsv)
            .iter()
            .filter(|(name, _, _)| escape_of(name).is_none_or(|e| lookup(table, e).is_none()))
            .map(|(name, _, count)| format!("{name} ({count} calls)"))
            .collect();
        assert!(missing.is_empty(), "escapes seen on hardware but unhandled: {missing:?}");
    }

    #[test]
    fn generated_sizes_match_the_t4_capture() {
        sizes_match(T4_580, DriverVersion::new(580, 178, 4), "the T4");
    }

    #[test]
    fn every_escape_the_t4_used_is_in_the_table() {
        all_escapes_known(T4_580, DriverVersion::new(580, 178, 4));
    }

    #[test]
    fn generated_sizes_match_the_a2000_capture() {
        sizes_match(A2000_615, DriverVersion::new(615, 71, 9), "the RTX A2000");
    }

    #[test]
    fn every_escape_the_a2000_used_is_in_the_table() {
        all_escapes_known(A2000_615, DriverVersion::new(615, 71, 9));
    }

    /// 615.71.09 deliberately has no profile of its own. If someone adds one,
    /// this test is how they find out that the A2000 capture stopped testing
    /// range selection and started testing an exact match instead.
    #[test]
    fn the_a2000_driver_is_served_by_range_selection() {
        let v = DriverVersion::new(615, 71, 9);
        assert!(
            !crate::versions::supported_versions().any(|p| p == v),
            "615.71.09 now has a profile of its own, so the A2000 fixture no longer \
             exercises range selection -- point it at a version that does, or drop this test"
        );
        assert!(std::ptr::eq(
            table_of(v),
            table_of(DriverVersion::new(595, 71, 5))
        ), "615.71.09 should resolve to the 595.71.05 table");
    }

    /// The strongest claim the two captures support together: a T4 on
    /// 580.178.04 and an A2000 on 615.71.09 -- different architectures, 35
    /// releases apart -- issue byte-identical parameter sizes for every escape
    /// both used. This is what justifies one table spanning that range. If a
    /// future capture breaks it, the versioning scheme needs a new profile, and
    /// this test is the thing that says so.
    #[test]
    fn the_two_captures_agree_on_every_shared_escape() {
        let t4 = parse(T4_580);
        let a2000 = parse(A2000_615);
        let mut shared = 0;

        for (name, t4_size, _) in &t4 {
            let Some((_, a_size, _)) = a2000.iter().find(|(n, _, _)| n == name) else {
                continue;
            };
            assert_eq!(
                t4_size, a_size,
                "{name}: T4/580.178.04 issued {t4_size} bytes, A2000/615.71.09 issued {a_size} \
                 -- the frontend ABI moved, and one table can no longer span both"
            );
            shared += 1;
        }

        assert!(shared >= 20, "only {shared} escapes were common to both captures");
    }
}
