//! What the guest driver expects to find on the bus.
//!
//! Every constant and layout here is a contract with `driver/virtio_gpu_nv.c`,
//! and each one has been wrong at least once. A mismatch is not a build error
//! in either half -- the device compiles, the driver compiles, and the guest
//! simply fails to probe -- so the agreement is asserted by tests that mirror
//! the driver's own `static_assert`s.

/// The virtio device ID the guest driver probes for.
///
/// Must match `VIRTIO_ID_GPU_NV` in `driver/virtio_gpu_nv.c`. This said 0x8042
/// while the driver bound 45, so a device advertising it would never have been
/// probed by its own guest driver.
pub const VIRTIO_ID_GPU_NV: u32 = 45;

/// Virtqueue count.
///
/// Two, not one: `nvgpu_probe()` calls `virtio_find_vqs(vdev, 2, ...)` for a
/// control queue and an event queue, and returns the error from that call. A
/// device offering one queue fails to probe before it reads a byte of config.
pub const NUM_QUEUES: usize = 2;

/// Guest requests, device replies.
pub const CONTROL_QUEUE: usize = 0;
/// Device-initiated notifications to the guest.
pub const EVENT_QUEUE: usize = 1;

/// Recommended virtqueue size.
pub const QUEUE_SIZE: u16 = 256;

/// Longest PCI address the driver will store, including its NUL.
pub const PCI_ADDR_LEN: usize = 16;
/// Bytes of `/proc/driver/nvidia/gpus/<addr>/information` carried per GPU.
pub const INFO_TEXT_LEN: usize = 1060;
/// Driver version string length in config space, including its NUL.
pub const DRIVER_VERSION_LEN: usize = 32;
/// GPU slots in config space. The driver reads at most this many.
pub const MAX_GPUS: usize = 8;
/// FD translation entries in config space.
pub const MAX_FD_TRANSLATIONS: usize = 16;

/// One GPU, as the guest driver reads it.
///
/// Mirrors `struct virtio_gpu_nv_gpu_slot`, which the driver asserts is 1088
/// bytes.
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct GpuSlot {
    /// Directory name under `/proc/driver/nvidia/gpus`, NUL-terminated.
    pub pci_addr: [u8; PCI_ADDR_LEN],
    /// The `N` in `/dev/nvidiaN`.
    pub minor: u32,
    /// Valid bytes in `info_text`.
    pub info_len: u32,
    pub padding: [u32; 1],
    /// Raw contents of that GPU's `information` file.
    pub info_text: [u8; INFO_TEXT_LEN],
}

impl Default for GpuSlot {
    fn default() -> Self {
        Self {
            pci_addr: [0; PCI_ADDR_LEN],
            minor: 0,
            info_len: 0,
            padding: [0; 1],
            info_text: [0; INFO_TEXT_LEN],
        }
    }
}

impl GpuSlot {
    /// Build a slot, truncating both strings to what the driver can hold.
    ///
    /// Truncating rather than failing is deliberate: a GPU whose information
    /// text is longer than the window is still a usable GPU, and refusing to
    /// describe it would take the whole device down over a cosmetic field.
    pub fn new(pci_addr: &str, minor: u32, info_text: &str) -> Self {
        let mut slot = Self {
            minor,
            ..Default::default()
        };
        // Leave room for the NUL the driver writes at the last byte.
        let addr = pci_addr.as_bytes();
        let n = addr.len().min(PCI_ADDR_LEN - 1);
        slot.pci_addr[..n].copy_from_slice(&addr[..n]);

        let info = info_text.as_bytes();
        let n = info.len().min(INFO_TEXT_LEN);
        slot.info_text[..n].copy_from_slice(&info[..n]);
        slot.info_len = n as u32;
        slot
    }
}

/// One ioctl the device wants the driver to rewrite file descriptors in.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C, packed)]
pub struct FdTranslation {
    /// The ioctl number this applies to.
    pub nr: u32,
    /// Where in the payload the descriptor sits.
    pub payload_offset: u32,
}

/// Device configuration space.
///
/// Mirrors `struct virtio_gpu_nv_config`, which the driver asserts is 8912
/// bytes with `num_fd_translations` at offset 8776.
///
/// This replaced a 24-byte struct whose first field was `num_gpus`. The driver
/// reads `num_gpus` from offset 32 and rejects zero, so it read past the end of
/// what the device served, got nothing, and failed to probe with `-EINVAL` --
/// a device and driver that disagreed about config space while both compiled
/// cleanly.
#[derive(Clone, Copy, Debug)]
#[repr(C, packed)]
pub struct VirtioGpuNvConfig {
    /// Host driver version, NUL-terminated, e.g. `615.71.09`.
    pub driver_version: [u8; DRIVER_VERSION_LEN],
    /// How many entries of `gpus` are valid. The driver requires 1..=248.
    pub num_gpus: u32,
    /// Capability bits.
    pub caps: u32,
    /// PCI device id per GPU.
    pub gpu_device_ids: [u32; MAX_GPUS],
    pub gpus: [GpuSlot; MAX_GPUS],
    pub num_fd_translations: u32,
    pub _pad: u32,
    pub fd_translations: [FdTranslation; MAX_FD_TRANSLATIONS],
}

impl Default for VirtioGpuNvConfig {
    fn default() -> Self {
        Self {
            driver_version: [0; DRIVER_VERSION_LEN],
            num_gpus: 0,
            caps: 0,
            gpu_device_ids: [0; MAX_GPUS],
            gpus: [GpuSlot::default(); MAX_GPUS],
            num_fd_translations: 0,
            _pad: 0,
            fd_translations: [FdTranslation::default(); MAX_FD_TRANSLATIONS],
        }
    }
}

impl VirtioGpuNvConfig {
    /// Build config space for a set of host GPUs.
    ///
    /// More than [`MAX_GPUS`] are truncated: the driver reads no further, so
    /// advertising a count it cannot index would point it at slots that were
    /// never written.
    pub fn new(driver_version: &str, gpus: &[GpuSlot]) -> Self {
        let mut cfg = Self::default();
        let v = driver_version.as_bytes();
        let n = v.len().min(DRIVER_VERSION_LEN - 1);
        cfg.driver_version[..n].copy_from_slice(&v[..n]);

        let n = gpus.len().min(MAX_GPUS);
        cfg.gpus[..n].copy_from_slice(&gpus[..n]);
        cfg.num_gpus = n as u32;
        cfg
    }

    /// Config space as the bytes a guest reads.
    pub fn as_bytes(&self) -> &[u8] {
        // Safe: `repr(C, packed)` with no padding and no pointers, so every
        // byte of the struct is initialised and meaningful.
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }

    /// Serve a config read, clamped to the struct.
    ///
    /// A read past the end yields fewer bytes rather than panicking: the guest
    /// chooses the offset and length, so neither may be trusted to be in range.
    pub fn read(&self, offset: u32, size: u32) -> Vec<u8> {
        let bytes = self.as_bytes();
        let start = (offset as usize).min(bytes.len());
        let end = start.saturating_add(size as usize).min(bytes.len());
        bytes[start..end].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// The driver asserts these exact numbers. If either side moves, a guest
    /// stops probing and nothing else says why.
    #[test]
    fn layout_matches_the_guest_driver() {
        assert_eq!(size_of::<GpuSlot>(), 1088, "gpu_slot size mismatch");
        assert_eq!(size_of::<VirtioGpuNvConfig>(), 8912, "config size mismatch");
        assert_eq!(
            offset_of!(VirtioGpuNvConfig, num_fd_translations),
            8776,
            "fd_translations offset mismatch"
        );
    }

    /// Every field the driver reads by a fixed offset.
    #[test]
    fn field_offsets_are_where_the_driver_reads_them() {
        assert_eq!(offset_of!(VirtioGpuNvConfig, driver_version), 0);
        assert_eq!(offset_of!(VirtioGpuNvConfig, num_gpus), 32);
        assert_eq!(offset_of!(VirtioGpuNvConfig, caps), 36);
        assert_eq!(offset_of!(VirtioGpuNvConfig, gpu_device_ids), 40);
        assert_eq!(offset_of!(VirtioGpuNvConfig, gpus), 72);
        assert_eq!(offset_of!(GpuSlot, minor), 16);
        assert_eq!(offset_of!(GpuSlot, info_len), 20);
        assert_eq!(offset_of!(GpuSlot, info_text), 28);
    }

    /// The driver fails probe on num_gpus == 0, so a device that serves a
    /// default config never comes up. This is what it looked like in a guest.
    #[test]
    fn a_default_config_would_be_rejected_by_the_driver() {
        let cfg = VirtioGpuNvConfig::default();
        let n = cfg.num_gpus;
        assert_eq!(n, 0, "a config with no GPUs must not claim any");
    }

    #[test]
    fn one_gpu_is_described_where_the_driver_looks() {
        let slot = GpuSlot::new("0000:01:00.0", 0, "Model: NVIDIA RTX A2000");
        let cfg = VirtioGpuNvConfig::new("615.71.09", &[slot]);
        let bytes = cfg.as_bytes();

        assert_eq!(&bytes[0..9], b"615.71.09");
        assert_eq!(u32::from_le_bytes(bytes[32..36].try_into().unwrap()), 1);
        assert_eq!(&bytes[72..84], b"0000:01:00.0");
        // info_len, at slot offset 20 within the slot array at 72.
        assert_eq!(
            u32::from_le_bytes(bytes[92..96].try_into().unwrap()),
            "Model: NVIDIA RTX A2000".len() as u32
        );
    }

    /// The driver NUL-terminates `pci_addr[15]` itself. A 16-byte address that
    /// filled the field would lose its last character there, so it is truncated
    /// to 15 on the way in and the driver's write is a no-op rather than a
    /// silent corruption.
    #[test]
    fn an_over_long_pci_address_keeps_room_for_its_terminator() {
        let slot = GpuSlot::new("0000:01:00.0:extra", 0, "");
        assert_eq!(slot.pci_addr[PCI_ADDR_LEN - 1], 0);
        assert_eq!(&slot.pci_addr[..15], b"0000:01:00.0:ex");
    }

    #[test]
    fn more_gpus_than_slots_are_truncated_not_over_claimed() {
        let many: Vec<_> = (0..12)
            .map(|i| GpuSlot::new(&format!("0000:0{i}:00.0"), i, ""))
            .collect();
        let cfg = VirtioGpuNvConfig::new("615.71.09", &many);
        let n = cfg.num_gpus;
        assert_eq!(n as usize, MAX_GPUS, "claimed more GPUs than it can describe");
    }

    #[test]
    fn a_read_past_the_end_is_clamped_rather_than_panicking() {
        let cfg = VirtioGpuNvConfig::default();
        assert_eq!(cfg.read(8900, 64).len(), 12);
        assert!(cfg.read(99_999, 16).is_empty());
        assert_eq!(cfg.read(0, 8912).len(), 8912);
    }
}
