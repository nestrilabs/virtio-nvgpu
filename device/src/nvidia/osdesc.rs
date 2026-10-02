//! Memory named by a CPU address, on the three routes that allow it.
//!
//! RM takes an address and a length and pins what is there for the GPU. The
//! address is read in the caller's address space, and the caller is this
//! backend: a guest that writes an address of its own has RM pin whatever of
//! *this* process's memory sits at that number. Nothing about the call says
//! it came from a guest, so nothing about RM's answer would look wrong.
//!
//! So the address is translated. The guest pins the pages behind it and sends
//! where they are; this file stitches them into one host span that aliases
//! exactly those pages ([`crate::guestmem`]), writes that span's address into
//! the parameter block in place of the guest's, and forwards the call. The
//! offsets of the address, the limit and the descriptor type are read from the
//! host release by [`abi::osdesc`] rather than written down here.
//!
//! The span lives as long as RM's object does, because RM pinned those pages
//! for that object's life and unmapping them earlier would leave RM pointing
//! at nothing. It is released when the object is freed, when the client that
//! holds it is freed, and on the file's close -- the same three moments the
//! guest driver releases its own pin.
//!
//! When the translation cannot be made -- no memory table from the transport,
//! no page runs from the guest, or runs that do not describe what was asked
//! for -- the call is refused rather than forwarded. The refusal goes back as
//! RM's own status in the parameter block and not as an ioctl errno, for the
//! reason the rest of the backend does the same: an errno makes NVIDIA's
//! userspace retry or hang, and a status is an answer it already knows how to
//! read. `NV_ERR_NOT_SUPPORTED` is what RM itself writes for a heap function
//! it does not serve.

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

    /// The route's layout and where, counted from the start of the parameter
    /// message, its fields sit.
    fn shape(&self, route: Registration, data_len: usize) -> Shape {
        let d = self
            .osdesc
            .expect("a route is only recognised when the table is there");
        match route {
            // The allocation parameters are the nested block, so everything in
            // them is offset by the outer struct the guest sent before it.
            // RM's status, though, is the outer struct's own.
            Registration::Alloc => Shape {
                route: d.alloc,
                params_at: data_len,
                status_at: NVOS64_STATUS,
                hmemory_at: 8,
            },
            Registration::AllocMemory => Shape {
                route: d.alloc_memory,
                params_at: 0,
                status_at: d.alloc_memory_status_at,
                hmemory_at: 8,
            },
            Registration::VidHeap => Shape {
                route: d.vid_heap,
                params_at: 0,
                status_at: d.vid_heap_status_at,
                hmemory_at: d.vid_heap_hmemory_at,
            },
        }
    }

    /// The guest's pages, as one address in this process.
    ///
    /// Everything that could make the span something other than exactly those
    /// pages is checked in [`crate::guestmem::stitch`]; what is checked here is
    /// that the guest said where its pages are at all, in the shape this
    /// backend reads.
    fn translate(
        &self,
        deep_in: Option<(usize, &[u8])>,
        want: u64,
    ) -> std::result::Result<crate::guestmem::Stitched, String> {
        let Some(ram) = self.guest_ram.as_deref() else {
            return Err(
                "the transport has not handed this backend the guest's memory table, \
                        so there is nowhere to look its pages up"
                    .into(),
            );
        };
        let Some((off, bytes)) = deep_in else {
            return Err(
                "the guest sent no page runs beside it, so where its pages are is not \
                        known. A guest driver older than this backend asks the way it always \
                        did, with an address only the guest can read"
                    .into(),
            );
        };
        if off as u32 != protocol::pageruns::PAGE_RUNS {
            return Err(format!(
                "the block beside it is not a page-run table: the guest put a deep pointer at \
                 byte {off} instead"
            ));
        }
        let Some(table) = protocol::pageruns::Runs::parse(bytes) else {
            return Err(format!(
                "the {} bytes beside it are not a page-run table",
                bytes.len()
            ));
        };
        let runs: Vec<crate::guestmem::Run> = table
            .iter()
            .map(|r| crate::guestmem::Run {
                gpa: r.gpa,
                len: r.len,
            })
            .collect();
        crate::guestmem::stitch(ram, &runs, want)
    }

    /// Serve a registration by address: translate it if it can be, refuse it
    /// if it cannot, and keep the translation alive for as long as RM's object.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn serve_registration(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        route: Registration,
        data_len: usize,
        param_in: &[u8],
        deep_in: Option<(usize, &[u8])>,
        resp_buf: &mut [u8],
    ) -> usize {
        let s = self.shape(route, data_len);
        let params = &param_in[s.params_at.min(param_in.len())..];

        // A block too short to hold the address it claims to carry is not a
        // registration this backend can read, whatever it meant to be.
        let (Some(addr), Some(length)) = (s.route.address(params), s.route.length(params)) else {
            return self.refuse_registration(
                cookie,
                route,
                &s,
                params,
                "the parameter block is too short to hold the address and the limit it is \
                 supposed to carry"
                    .to_string(),
                param_in,
                resp_buf,
            );
        };

        // Only a user virtual address is something the guest can pin and this
        // backend can rebuild. The other descriptor types name a kernel
        // address, a physical address, a file handle, a dma-buf or an SG
        // table, and every one of those numbers would be read in *this*
        // process: a file handle from a guest picks out whatever this backend
        // has open at that number. RM's own answer for a virtual address on
        // this route is NV_ERR_NOT_SUPPORTED, so refusing the rest here costs
        // a guest nothing it could otherwise have had.
        let d = self
            .osdesc
            .expect("a route is only recognised when the table is there");
        if s.route
            .desc_type(params)
            .is_some_and(|t| t != d.virtual_address)
        {
            return self.refuse_registration(
                cookie,
                route,
                &s,
                params,
                "it is not a user virtual address, and every other kind of descriptor names \
                 something in this process rather than in the guest"
                    .to_string(),
                param_in,
                resp_buf,
            );
        }

        let stitched = match self.translate(deep_in, length) {
            Ok(st) => st,
            Err(why) => {
                return self
                    .refuse_registration(cookie, route, &s, params, why, param_in, resp_buf);
            }
        };

        log::debug!(
            "{}: guest address {:#x}+{:#x} is host address {:#x}",
            route.what(),
            addr,
            length,
            stitched.addr(),
        );

        // The one change made to what the guest sent: RM reads the address in
        // this process, so it gets this process's.
        let mut forwarded = param_in.to_vec();
        let at = s.params_at + s.route.address_at;
        forwarded[at..at + 8].copy_from_slice(&stitched.addr().to_le_bytes());

        let n = match route {
            Registration::Alloc => self.dispatch_nested(
                cookie, host_fd, request, &forwarded, resp_buf, 48, 16, 32, None, None,
            ),
            Registration::AllocMemory => self.dispatch_fd_carrying(
                cookie,
                host_fd,
                request,
                abi::ioctl::NV_ESC_RM_ALLOC_MEMORY,
                &forwarded,
                resp_buf,
            ),
            Registration::VidHeap => {
                self.dispatch_simple(cookie, host_fd, request, &forwarded, resp_buf)
            }
        };

        // RM's own answer decides whether the span is kept. Anything but
        // NV_OK means nothing was registered, and `stitched` falling out of
        // scope here unmaps it.
        let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        if n < head || read_struct::<MsgHeader>(resp_buf, 0).status != 0 {
            return n;
        }
        let out = &resp_buf[head..n];
        let word = |at: usize| -> Option<u32> {
            out.get(at..at + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        if word(s.status_at) != Some(NV_OK) {
            return n;
        }
        // hRoot is the first field of all three parameter structs.
        let (Some(client), Some(object)) = (word(0), word(s.hmemory_at)) else {
            return n;
        };
        log::debug!(
            "{}: {:#x} bytes held for object {:#x}/{:#x}",
            route.what(),
            length,
            client,
            object,
        );
        self.registrations
            .insert((client, object), (self.current_handle as u64, stitched));
        self.registrations_served += 1;
        self.registrations_peak = self.registrations_peak.max(self.registrations.len());
        n
    }

    /// Release what a freed object held, and everything a freed client held.
    ///
    /// RM frees an object's children with it, and this does not follow that:
    /// memory registered under a device and released by freeing the device
    /// stays mapped until the client or the file goes. The guest driver has
    /// the same gap, so the two halves agree, and the file's close closes it.
    pub(super) fn release_registrations(&mut self, client: u32, object: u32) {
        let whole_client = client == object;
        self.registrations.retain(|(c, o), _| {
            let mine = *c == client && (whole_client || *o == object);
            if mine {
                log::debug!("free {client:#x}/{o:#x}: releasing the guest memory it registered");
            }
            !mine
        });
    }

    /// Release everything a closing file still holds.
    pub(super) fn drop_registrations_for_file(&mut self, file: u64) {
        self.registrations.retain(|(c, o), (f, _)| {
            let mine = *f == file;
            if mine {
                log::debug!(
                    "close handle={file}: releasing the guest memory registered as {c:#x}/{o:#x}"
                );
            }
            !mine
        });
    }

    /// Answer a registration by address without calling the host.
    #[allow(clippy::too_many_arguments)]
    fn refuse_registration(
        &mut self,
        cookie: u64,
        route: Registration,
        s: &Shape,
        params: &[u8],
        why: String,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        let d = self
            .osdesc
            .expect("a route is only recognised when the table is there");

        // Said in full the first time, because the next person to see this in
        // a log will be looking at a CUDA run that stopped.
        let kind = s
            .route
            .desc_type(params)
            .map(|t| d.type_name(t).unwrap_or("an unknown descriptor type"))
            .unwrap_or("NVOS32_DESCRIPTOR_TYPE_VIRTUAL_ADDRESS, implied by the class");
        self.note_allow_refusal(
            route.what().to_string(),
            format!(
                "it registers memory by a CPU address ({kind}), and an address from a guest \
                 names this process's memory, not the guest's. {} bytes at {:#x} were asked \
                 for, and {why}.",
                s.route.length(params).unwrap_or(0),
                s.route.address(params).unwrap_or(0),
            ),
        );

        let mut out = param_in.to_vec();
        let Some(slot) = out.get_mut(s.status_at..s.status_at + 4) else {
            // Too short to hold a status, so there is nowhere to put the
            // answer and an errno is all that is left.
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        };
        slot.copy_from_slice(&NV_ERR_NOT_SUPPORTED.to_le_bytes());
        self.write_ioctl_resp(resp_buf, cookie, &out)
    }
}

/// Where one route keeps its fields, counted from the start of the parameter
/// message the guest sent.
struct Shape {
    route: abi::osdesc::Route,
    params_at: usize,
    status_at: usize,
    hmemory_at: usize,
}
