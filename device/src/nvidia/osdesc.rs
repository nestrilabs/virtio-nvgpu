//! Memory named by a CPU address, on the three routes that allow it.
//!
//! RM takes an address and a length and pins what is there for the GPU. The
//! address is read in the caller's address space, and the caller is this
//! backend: a guest that writes an address of its own has RM pin whatever of
//! *this* process's memory sits at that number. Nothing about the call says
//! it came from a guest, so nothing about RM's answer would look wrong.
//!
//! Until the translation exists -- the guest pinning its own pages and the
//! backend stitching them into a host address that aliases exactly those
//! pages, which is M6 -- the only safe answer is no, under every capability.
//! That is what this file does, on all three routes, with the offsets read
//! from the host release by [`abi::osdesc`] rather than written down here.
//!
//! The refusal goes back as RM's own status in the parameter block and not as
//! an ioctl errno, for the reason the rest of the backend does the same: an
//! errno makes NVIDIA's userspace retry or hang, and a status is an answer it
//! already knows how to read. `NV_ERR_NOT_SUPPORTED` is what RM itself writes
//! for a heap function it does not serve.

use super::*;

/// `NV_ERR_NOT_SUPPORTED` (nvstatuscodes.h).
pub(super) const NV_ERR_NOT_SUPPORTED: u32 = 0x56;

/// Which of the three routes a call arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Registration {
    /// `NV_ESC_RM_ALLOC`, class `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`.
    Alloc,
    /// `NV_ESC_RM_ALLOC_MEMORY`, same class, different struct.
    AllocMemory,
    /// `NV_ESC_RM_VID_HEAP_CONTROL`, function `ALLOC_OS_DESCRIPTOR`.
    VidHeap,
}

impl Registration {
    fn what(self) -> &'static str {
        match self {
            Self::Alloc => "RM_ALLOC of NV01_MEMORY_SYSTEM_OS_DESCRIPTOR",
            Self::AllocMemory => "RM_ALLOC_MEMORY of NV01_MEMORY_SYSTEM_OS_DESCRIPTOR",
            Self::VidHeap => "VID_HEAP_CONTROL ALLOC_OS_DESCRIPTOR",
        }
    }
}

impl NvidiaBackend {
    /// Whether this call registers memory by a CPU address, and on which route.
    ///
    /// `params` is the block the route keeps its own fields in: for `RM_ALLOC`
    /// that is the nested allocation parameters, and for the other two the
    /// flat parameter struct.
    pub(super) fn registration_by_address(
        &self,
        escape: u32,
        class: Option<u32>,
        params: &[u8],
    ) -> Option<Registration> {
        use abi::ioctl::*;
        let d = self.osdesc?;
        match escape {
            NV_ESC_RM_ALLOC if class == Some(d.class) => Some(Registration::Alloc),
            NV_ESC_RM_ALLOC_MEMORY => {
                // This route carries its class inside the parameters, so the
                // caller cannot have read it for us.
                let at = d.alloc_memory_class_at;
                let c = params.get(at..at + 4)?;
                (u32::from_le_bytes(c.try_into().unwrap()) == d.class)
                    .then_some(Registration::AllocMemory)
            }
            // The parameters are a union. Every other function is an ordinary
            // heap operation whose bytes at the address offset are something
            // else entirely, so the function is read first and nothing else.
            NV_ESC_RM_VID_HEAP_CONTROL => d
                .vid_heap_registers_address(params)
                .then_some(Registration::VidHeap),
            _ => None,
        }
    }

    /// Answer a registration by address without calling the host.
    pub(super) fn refuse_registration(
        &mut self,
        cookie: u64,
        route: Registration,
        params: &[u8],
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        let d = self
            .osdesc
            .expect("a route is only recognised when the table is there");
        let r = match route {
            Registration::Alloc => d.alloc,
            Registration::AllocMemory => d.alloc_memory,
            Registration::VidHeap => d.vid_heap,
        };

        // Said in full the first time, because the next person to see this in
        // a log will be looking at a CUDA run that stopped.
        let kind = r
            .desc_type(params)
            .map(|t| d.type_name(t).unwrap_or("an unknown descriptor type"))
            .unwrap_or("NVOS32_DESCRIPTOR_TYPE_VIRTUAL_ADDRESS, implied by the class");
        self.note_allow_refusal(
            route.what().to_string(),
            format!(
                "it registers memory by a CPU address ({kind}), and an address from a guest \
                 names this process's memory, not the guest's. {} bytes at {:#x} were asked \
                 for. Translating them is M6; until then this is refused under every \
                 capability.",
                r.length(params).unwrap_or(0),
                r.address(params).unwrap_or(0),
            ),
        );

        // Where RM would have written its answer.
        let status_at = match route {
            Registration::Alloc => NVOS64_STATUS,
            Registration::AllocMemory => d.alloc_memory_status_at,
            Registration::VidHeap => d.vid_heap_status_at,
        };
        let mut out = param_in.to_vec();
        let Some(slot) = out.get_mut(status_at..status_at + 4) else {
            // Too short to hold a status, so there is nowhere to put the
            // answer and an errno is all that is left.
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        };
        slot.copy_from_slice(&NV_ERR_NOT_SUPPORTED.to_le_bytes());
        self.write_ioctl_resp(resp_buf, cookie, &out)
    }
}
