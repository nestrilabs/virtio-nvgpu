//! Checking generated tables against a real driver.
//!
//! `gen/fixtures/*.tsv` records the ioctl parameter sizes actually observed on
//! hardware, captured with `nvidia_sniffer` under `LD_PRELOAD`. The tables in
//! `versions/` are derived from gVisor's nvproxy; the fixtures are derived from
//! a running GPU. Neither is checked against the other anywhere else, so a
//! disagreement here means one of them is wrong about a driver we claim to
//! support -- which is exactly the failure that is otherwise found only at
//! runtime, in a guest, as a silently truncated ioctl.

#[cfg(test)]
mod tests {
    use crate::ioctl;
    use crate::version::DriverVersion;
    use crate::versions::{lookup, table_for, IoctlKind};

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

    #[test]
    fn generated_sizes_match_the_t4_capture() {
        let table = table_for(DriverVersion::new(580, 178, 4)).expect("580.178.04 has a profile");
        let mut checked = 0;

        for (name, observed, _count) in parse(T4_580) {
            let escape = escape_of(name).unwrap_or_else(|| panic!("fixture names {name}, which gen/src/ioctl.rs does not define"));
            let entry = lookup(table, escape)
                .unwrap_or_else(|| panic!("{name} was observed on hardware but is absent from the 580.178.04 table"));

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
                        .unwrap_or_else(|| panic!("{name} has no size in the generated table"));
                    assert_eq!(
                        expected as i64, observed,
                        "{name}: table says {expected} bytes, the T4 running 580.178.04 issued {observed}"
                    );
                    checked += 1;
                }
            }
        }

        // Guard against the fixture silently emptying out.
        assert!(checked >= 15, "only {checked} sizes were actually compared");
    }

    #[test]
    fn every_escape_the_hardware_used_is_in_the_table() {
        let table = table_for(DriverVersion::new(580, 178, 4)).expect("580.178.04 has a profile");
        let missing: Vec<_> = parse(T4_580)
            .iter()
            .filter(|(name, _, _)| {
                escape_of(name).is_none_or(|e| lookup(table, e).is_none())
            })
            .map(|(name, _, count)| format!("{name} ({count} calls)"))
            .collect();
        assert!(missing.is_empty(), "escapes seen on hardware but unhandled: {missing:?}");
    }
}
