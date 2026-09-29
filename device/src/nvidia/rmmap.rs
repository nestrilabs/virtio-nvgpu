// SPDX-License-Identifier: Apache-2.0
//! RM_MAP_MEMORY, UPDATE_DEVICE_MAPPING_INFO and RM_UNMAP_MEMORY: a
//! mapping RM makes of device memory, placed in the window and recorded
//! for the guest to name it by.

#![forbid(unsafe_code)]

use super::*;

impl NvidiaBackend {
    pub(super) fn dispatch_update_device_mapping_info(
        &mut self,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
    ) -> V1 {
        log::debug!(
            "UPDATE_DEVICE_MAPPING_INFO: ENTERED, host_fd={}, param_in.len={}",
            host_fd,
            param_in.len()
        );

        if param_in.len() < NVOS56_SIZE {
            return Err(libc::EINVAL);
        }
        let word = |at| le::u32_at(param_in, at).expect("checked above");
        let addr = |at| le::u64_at(param_in, at).expect("checked above");
        let (h_client, h_memory) = (word(NVOS56_H_CLIENT), word(NVOS56_H_MEMORY));
        let old_cpu_addr = addr(NVOS56_P_OLD_CPU_ADDRESS);
        let new_cpu_addr = addr(NVOS56_P_NEW_CPU_ADDRESS);

        log::debug!(
            "UPDATE_DEVICE_MAPPING_INFO: client={:#x} mem={:#x} old={:#x} new={:#x}",
            h_client,
            h_memory,
            old_cpu_addr,
            new_cpu_addr
        );

        // Where the library mapped what RM_MAP_MEMORY armed (`pNew`), told
        // to RM by the address it knew the mapping by until now (`pOld`):
        // the window offset the map returned, or the address an earlier
        // UPDATE gave it. RM moves its record of the mapping from one to
        // the other (osapi.c RmUpdateDeviceMappingInfo), and the library
        // names the mapping by `pNew` from then on -- UNMAP_MEMORY included.
        //
        // The mapping is found by the object, the process and `pOld`
        // together (`MmapContext::find`): by the object alone, two mappings
        // of it were one, and whichever came first was the one moved. If
        // `pOld` names none of the caller's, and it has exactly one mapping
        // of the object, that one is meant, as this backend always assumed.
        let caller = self.current_owner;
        let key = self
            .active_maps
            .find(h_client, h_memory, caller, old_cpu_addr)
            .or_else(|| {
                let k = self.active_maps.only_mapping_of(h_client, h_memory, caller);
                if k.is_some() {
                    log::debug!(
                        "UPDATE_DEVICE_MAPPING_INFO: pOld {old_cpu_addr:#x} names no mapping of \
                         client {h_client:#x} memory {h_memory:#x}; taking its only one"
                    );
                }
                k
            });

        // The host is handed its own address for the mapping, as both pOld
        // and pNew: its record stays where the backend's mapping is, which
        // did not move. A mapping we have no record of gets 0, not the
        // guest's value: the host takes both as addresses in this process
        // (guestptr.rs), and RM finds no mapping at 0.
        let host_old = key
            .and_then(|k| self.active_maps.find_by_offset(k))
            .map_or(0, |e| e.host_p_linear_address);
        if key.is_some() {
            log::debug!(
                "UPDATE_DEVICE_MAPPING_INFO: translated old {:#x} → host {:#x}",
                old_cpu_addr,
                host_old
            );
        }

        // pOldCpuAddress and pNewCpuAddress: the host VA (the host mapping
        // didn't move), and the caller's own in the reply. RM only reads
        // pOld/pNew (escape.c:857-876 takes them into locals and writes
        // nothing but `status`), and nvidia.ko copies the whole argument back
        // (nv.c:2834), so a native caller reads back what it passed -- not
        // zero, and never the host VA we put there for the call.
        let mut a = Arena::new();
        let built = self
            .top_block(&mut a, request, param_in, &crate::guestptr::Plan::default())
            .and_then(|top| {
                for off in [NVOS56_P_OLD_CPU_ADDRESS, NVOS56_P_NEW_CPU_ADDRESS] {
                    a.value(top, off, 8, Restore::Yes)?;
                    a.set_value(top, off, host_old)?;
                }
                Ok(top)
            });
        let top = built?;
        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            log::warn!(
                "UPDATE_DEVICE_MAPPING_INFO: host ioctl failed: errno={}",
                errno
            );
            return Err(errno);
        }
        let param_buf = a.reply(top)[..param_in.len()].to_vec();
        let status = le::u32_at(&param_buf, NVOS56_STATUS).expect("checked above");
        log::debug!("UPDATE_DEVICE_MAPPING_INFO: host status=0x{:x}", status);
        // What RM would now know the mapping by, once it has said yes.
        if status == NV_OK
            && let Some(k) = key
        {
            self.active_maps.set_guest_va(k, new_cpu_addr);
        }
        Ok(IoctlOut::ok(param_buf))
    }

    pub(super) fn dispatch_map_memory(
        &mut self,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        plan: &crate::guestptr::Plan<'_>,
    ) -> V1 {
        if param_in.len() < NVOS33_WITH_FD_SIZE {
            return Err(libc::EINVAL);
        }

        // --- Step 1: Translate embedded FD (guest handle → host fd) ---

        let f = FdField {
            at: NVOS33_WITH_FD_FD,
            width: 4,
            accept: FdAccept::Device,
            none: FdNone::Never,
        };
        let (guest_fd_handle, host_map) = match self.fd_field(param_in, f) {
            Ok(FdIn::File { handle, fd }) => (handle, fd),
            _ => {
                log::warn!("NV_ESC_RM_MAP_MEMORY: its descriptor is none of our devices");
                return Err(libc::EBADF);
            }
        };
        let host_map_fd = host_map.as_raw_fd();

        // A host fd is single-use for mapping. The host will refuse this with
        // NV_ERR_STATE_IN_USE; say so here, because that status on its own
        // sends you looking at the unmap path, which is not the problem.
        if self.active_maps.fd_has_mapping(guest_fd_handle) {
            log::warn!(
                "NV_ESC_RM_MAP_MEMORY: handle {guest_fd_handle} already carries a mapping; \
                 a host fd cannot carry two, so the host will return NV_ERR_STATE_IN_USE"
            );
        }

        // The host's copy: the guest's NVOS33, the descriptor field holding
        // our descriptor of that file, pLinearAddress (the plan's) held at 0.
        let mut a = Arena::new();
        let built = self
            .top_block(&mut a, request, param_in, plan)
            .and_then(|top| {
                let file = FdIn::File {
                    handle: guest_fd_handle,
                    fd: host_map,
                };
                declare_fd(&mut a, top, f, &file).map(|_| top)
            });
        let top = built?;

        // --- Step 2: reserve the window extent, before RM maps anything ---
        //
        // A mapping is one transaction: the extent, the host's mapping and
        // the placement all happen, or none does. The extent is the part
        // that can be refused -- the zone is full, or this process holds its
        // share of it (quota.rs) -- so it comes first, and a refusal leaves
        // the host with nothing mapped. It used to come after RM, and each
        // refusal left a host mapping made, recorded nowhere, until the
        // memory was freed.
        //
        // Charged to whoever opened the file the mapping is armed on: the
        // process that will map it. The zone is the one the mapping will
        // most likely need (`rm_mapping_pgprot_before`); RM's reply decides,
        // and a reply that wants another zone moves the reservation there.
        let owner = match self.handles.owner(guest_fd_handle) {
            crate::quota::Owner::Unknown => self.current_owner,
            o => o,
        };
        let word = |at| le::u32_at(param_in, at).expect("checked above");
        let (h_client, h_memory) = (word(NVOS33_H_CLIENT), word(NVOS33_H_MEMORY));
        let asked = le::u64_at(param_in, NVOS33_LENGTH).expect("checked above");
        let expected = self.rm_mapping_pgprot_before(h_client, h_memory);
        // A zero length can be given no extent; RM refuses it on its own,
        // and says so in `status`.
        //
        // A length no zone could hold -- up to u64::MAX, the guest's to
        // choose -- is refused before any arithmetic on it. Either refusal
        // is RM's own out-of-memory answer, NV_ERR_NO_MEMORY in the status
        // with the ioctl succeeding, which is how libnvidia learns it ran
        // out; a bare errno reads as a generic OS failure.
        let reserved = if asked == 0 {
            None
        } else {
            let got = if asked > self.shm.largest_zone() {
                Err(DeviceError::Io(std::io::Error::other(format!(
                    "{asked:#x} bytes is more than any zone holds"
                ))))
            } else {
                self.alloc_zone(asked, expected, owner)
            };
            match got {
                Ok(r) => Some(r),
                Err(e) => {
                    log::warn!("NV_ESC_RM_MAP_MEMORY: SHM alloc failed: {}", e);
                    let out = nvos::with_status(param_in, NVOS33_STATUS, NV_ERR_NO_MEMORY);
                    return Ok(IoctlOut::ok(out));
                }
            }
        };
        // Give the reservation back, on a path where it will not be used.
        let unreserve = |be: &mut Self, r: Option<crate::shm::ShmRegion>| {
            if let Some(r) = r
                && let Err(e) = be.shm.free(&r)
            {
                log::warn!("NV_ESC_RM_MAP_MEMORY: freeing the unused region: {e}");
            }
        };

        // --- Step 3: Call host ioctl ---

        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            log::warn!("NV_ESC_RM_MAP_MEMORY: host ioctl failed: errno={}", errno);
            unreserve(self, reserved);
            return Err(errno);
        }

        // --- Step 4: Check RM status and read updated fields ---

        // The guest handle back in the descriptor field, regardless of status.
        let mut param_buf = a.reply(top)[..param_in.len()].to_vec();
        drop(a);
        let reply = |at| le::u32_at(&param_buf, at).expect("the block as sent");
        let rm_status = reply(NVOS33_STATUS);

        if rm_status != NV_OK {
            // RM returned an error status (NV_OK == 0).
            // Forward the params back so the guest can read the status field.
            log::debug!("NV_ESC_RM_MAP_MEMORY: RM status 0x{:x}", rm_status);
            unreserve(self, reserved);
            return Ok(IoctlOut::ok(param_buf));
        }

        // NVOS33.pLinearAddress: the host's own address for the mapping,
        // which RM knows it by (in UPDATE_DEVICE_MAPPING_INFO and UNMAP), and
        // which undoes it if what follows fails.
        let host_p_linear =
            le::u64_at(&param_buf, NVOS33_P_LINEAR_ADDRESS).expect("the block as sent");
        let length = le::u64_at(&param_buf, NVOS33_LENGTH).expect("the block as sent");
        let flags = reply(NVOS33_FLAGS);

        // --- Step 5: the memory type the host maps it with ---
        //
        // Not the caching type alone: that is real only for video memory (see
        // `rm_mapping_pgprot`), and reading it for everything put system
        // memory and the doorbell registers write-combining.
        let pgprot = self.rm_mapping_pgprot(flags, h_client, h_memory);
        // And whether it can be written at all: RM makes some mappings
        // read-only, and placing one writable would let a guest write stop the
        // VM (see `shm::host_mapping_writable`).
        let writable = crate::shm::host_mapping_writable(host_map_fd, length, 0);
        if !writable {
            log::debug!(
                "NV_ESC_RM_MAP_MEMORY: client {h_client:#x} memory {h_memory:#x} is read-only \
                 on the host; placed read-only"
            );
        }

        // --- Step 6: the extent RM's answer calls for ---
        //
        // Nearly always the reservation. When RM mapped the memory with
        // another type than expected (or, never seen, another length), the
        // extent moves to that type's zone; if it does not fit there, the
        // host's mapping is undone and nothing is left of the call.
        let region = match reserved {
            Some(r) if pgprot == expected && length == asked => r,
            other => {
                unreserve(self, other);
                match self.alloc_zone(length, pgprot, owner) {
                    Ok(r) => r,
                    Err(e) => {
                        log::warn!("NV_ESC_RM_MAP_MEMORY: SHM alloc failed: {}", e);
                        self.undo_rm_map(host_fd, &param_buf, host_p_linear);
                        // RM's out-of-memory answer, as for a refused
                        // reservation; the caller's own block.
                        let out = nvos::with_status(param_in, NVOS33_STATUS, NV_ERR_NO_MEMORY);
                        return Ok(IoctlOut::ok(out));
                    }
                }
            }
        };

        // --- Step 7: place the device fd in the window ---
        //
        // Placed by the transport, not here: see `WindowPlacer`. Without one
        // the mapping exists on the host and is unreachable from the guest, so
        // the honest answer is to fail the call rather than return an address
        // that names nothing -- and to undo the host's mapping with it.
        let placed = match self.window.as_ref() {
            None => {
                log::warn!(
                    "NV_ESC_RM_MAP_MEMORY: no shared window, so device memory cannot be \
                     addressed by the guest"
                );
                Err(NV_ERR_NOT_SUPPORTED)
            }
            Some(window) => window
                .place(region.offset, length, host_map_fd, 0, writable)
                .map_err(|e| {
                    log::error!("NV_ESC_RM_MAP_MEMORY: placing in the window failed: {}", e);
                    NV_ERR_NO_MEMORY
                }),
        };
        // RM's statuses in the caller's own block, the ioctl succeeding, as
        // RM answers a mapping it cannot make: an errno reads to
        // libnvidia as a generic OS failure.
        if let Err(status) = placed {
            unreserve(self, Some(region));
            self.undo_rm_map(host_fd, &param_buf, host_p_linear);
            let out = nvos::with_status(param_in, NVOS33_STATUS, status);
            return Ok(IoctlOut::ok(out));
        }

        log::debug!(
            "dispatch_map_memory: returning shm_offset=0x{:x} shm_length=0x{:x} pgprot={:?}{}",
            region.offset,
            length,
            region.pgprot,
            if writable { "" } else { " read-only" }
        );

        // --- Step 8: record the mapping ---
        //
        // The host wrote its own address into pLinearAddress; it is kept for
        // UPDATE_DEVICE_MAPPING_INFO and UNMAP, and the guest is given the
        // window offset in its place. The guest library stores that value
        // and quotes it back until it tells RM where it mapped the file
        // (UPDATE_DEVICE_MAPPING_INFO), and by that address afterwards; the
        // entry answers to both (`MmapContext::find`).
        //
        // The handle is the key the mmap that follows will be found by, so it
        // is the one field worth naming in the log: a mapping that is armed
        // against one file and consumed on another is the whole failure mode.
        log::debug!(
            "MAP_MEMORY: armed on handle {} (shm_off={:#x}) → host_va={:#x} client={:#x} mem={:#x}",
            guest_fd_handle,
            region.offset,
            host_p_linear,
            h_client,
            h_memory
        );

        let (region_offset, pgprot) = (region.offset, region.pgprot);
        self.active_maps.insert(
            region_offset,
            crate::mmap::MmapEntry {
                host_p_linear_address: host_p_linear,
                shm_length: length,
                h_client,
                h_memory,
                map_fd_handle: guest_fd_handle,
                region,
                writable,
                mapping_id: 0,
                refs: 0,
                caller: self.current_owner,
                guest_va: None,
            },
        );

        // Replace host VA with SHM offset in pLinearAddress — this is what
        // the guest sees. It's not a real pointer; the guest driver uses the
        // SHM metadata (shm_offset/shm_length/pgprot in IoctlResp) for mmap,
        // and the library stores this value to pass back at unmap time.
        le::put_u64(&mut param_buf, NVOS33_P_LINEAR_ADDRESS, region_offset)
            .expect("the block as sent");

        // --- Step 9: Respond ---
        //
        // Only the parameter buffer goes back. The SHM offset and length do not
        // ride along on the ioctl reply: the guest maps by issuing a separate
        // Mmap message on the file this mapping was armed on, and that reply
        // is what carries the placement, the memory type decided above and
        // whether it is read-only. An earlier reply struct carried the
        // placement here, which the guest driver never read.
        log::debug!(
            "map_memory: SHM {:#x}+{:#x}, pgprot {pgprot:?}",
            region_offset,
            length
        );
        Ok(IoctlOut::ok(param_buf))
    }

    /// Undo an RM_MAP_MEMORY RM said yes to, when the backend cannot give
    /// the guest what it mapped (no extent in the zone RM's answer calls
    /// for, or no placement): NV_ESC_RM_UNMAP_MEMORY of the same object at
    /// the host's own address, on the same file, as the guest's own unmap
    /// would be sent. `nvos33` is the reply as RM left it.
    ///
    /// The descriptor the mapping was armed on stays spent (a host fd
    /// carries one mapping in its life, `repeated_map_unmap_does_not_
    /// exhaust_the_zone`); the guest closes it, as it would after any
    /// failed map.
    pub(super) fn undo_rm_map(&self, host_fd: RawFd, nvos33: &[u8], host_p_linear: u64) {
        // The object from the map, flags 0 (a user mapping).
        let mut p = [0u8; NVOS34_SIZE];
        for (to, from) in [
            (NVOS34_H_CLIENT, NVOS33_H_CLIENT),
            (NVOS34_H_DEVICE, NVOS33_H_DEVICE),
            (NVOS34_H_MEMORY, NVOS33_H_MEMORY),
        ] {
            let v = le::u32_at(nvos33, from).unwrap_or(0);
            le::put_u32(&mut p, to, v).expect("in the block");
        }
        let request = abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_UNMAP_MEMORY, p.len() as u32);
        let mut a = Arena::new();
        let built = self
            .top_block(&mut a, request, &p, &crate::guestptr::Plan::default())
            .and_then(|top| {
                a.value(top, NVOS34_P_LINEAR_ADDRESS, 8, Restore::No)?;
                a.set_value(top, NVOS34_P_LINEAR_ADDRESS, host_p_linear)?;
                Ok(top)
            });
        let status = built.and_then(|top| {
            self.host_call(&mut a, host_fd, request, top)?;
            Ok(le::u32_at(&a.reply(top), NVOS34_STATUS).unwrap_or(0))
        });
        match status {
            Ok(0) => log::info!(
                "NV_ESC_RM_MAP_MEMORY: undone on the host (client {:#x} memory {:#x})",
                le::u32_at(&p, NVOS34_H_CLIENT).unwrap_or(0),
                le::u32_at(&p, NVOS34_H_MEMORY).unwrap_or(0),
            ),
            Ok(s) => log::warn!(
                "NV_ESC_RM_MAP_MEMORY: the host would not undo the mapping (RM status {s:#x}); \
                 it lasts until the memory is freed"
            ),
            Err(errno) => log::warn!(
                "NV_ESC_RM_MAP_MEMORY: undoing the mapping on the host failed (errno {errno}); \
                 it lasts until the memory is freed"
            ),
        }
    }

    pub(super) fn dispatch_unmap_memory(
        &mut self,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
    ) -> V1 {
        if param_in.len() < NVOS34_SIZE {
            return Err(libc::EINVAL);
        }
        let word = |at| le::u32_at(param_in, at).expect("checked above");
        let (h_client, h_memory) = (word(NVOS34_H_CLIENT), word(NVOS34_H_MEMORY));
        let guest_linear = le::u64_at(param_in, NVOS34_P_LINEAR_ADDRESS).expect("checked above");

        // guest_linear is what the caller knows the mapping by: the window
        // offset the map wrote into pLinearAddress, or the address an
        // UPDATE_DEVICE_MAPPING_INFO moved it to (`MmapContext::find`), for
        // this object and this process only.
        let found = self
            .active_maps
            .find(h_client, h_memory, self.current_owner, guest_linear)
            .and_then(|k| self.active_maps.remove(k));
        let entry = match found {
            Some(e) => e,
            None => {
                log::warn!(
                    "UNMAP_MEMORY: no mapping for pLinearAddress={:#x} \
                     (hClient={:#x}, hMemory={:#x})",
                    guest_linear,
                    h_client,
                    h_memory
                );
                // Forwarded so RM answers it, but with no address: the
                // guest's value is not one of ours, and the host looks the
                // mapping up by the address it is given (guestptr.rs). RM
                // finds none at 0 and says so in `status`; the caller reads
                // back its own value.
                let mut a = Arena::new();
                let built = self
                    .top_block(&mut a, request, param_in, &crate::guestptr::Plan::default())
                    .and_then(|top| {
                        a.value(top, NVOS34_P_LINEAR_ADDRESS, 8, Restore::Yes)
                            .map(|_| top)
                    });
                let top = built?;
                self.host_call(&mut a, host_fd, request, top)?;
                return Ok(IoctlOut::ok(a.reply(top)[..param_in.len()].to_vec()));
            }
        };

        log::debug!(
            "UNMAP_MEMORY: {:#x} (shm_off={:#x}) → host_va={:#x} (client={:#x}, mem={:#x})",
            guest_linear,
            entry.region.offset,
            entry.host_p_linear_address,
            h_client,
            h_memory
        );

        // The real host pLinearAddress for the host ioctl, a value of ours.
        let mut a = Arena::new();
        let built = self
            .top_block(&mut a, request, param_in, &crate::guestptr::Plan::default())
            .and_then(|top| {
                // RM does not write NVOS34.pLinearAddress and nvidia.ko
                // copies the block back, so the caller reads its own value,
                // as on the not-found path and as L-6 did for
                // UPDATE_DEVICE_MAPPING_INFO.
                a.value(top, NVOS34_P_LINEAR_ADDRESS, 8, Restore::Yes)?;
                a.set_value(top, NVOS34_P_LINEAR_ADDRESS, entry.host_p_linear_address)?;
                Ok(top)
            });
        let top = match built {
            Ok(t) => t,
            Err(e) => {
                self.active_maps.insert(entry.region.offset, entry);
                return Err(e);
            }
        };
        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            log::warn!("UNMAP_MEMORY: host ioctl failed: errno={}", errno);
            // Restore the entry since unmap didn't happen
            self.active_maps.insert(entry.region.offset, entry);
            return Err(errno);
        }
        let param_buf = a.reply(top)[..param_in.len()].to_vec();
        drop(a);

        let status = le::u32_at(&param_buf, NVOS34_STATUS).expect("checked above");
        log::debug!("UNMAP_MEMORY: host status=0x{:x}", status);

        if status == NV_OK {
            // Host unmap succeeded -- empty the window range and return the
            // extent to its zone, so the space can serve a later mapping. The
            // withdraw matters as much as the free: without it the VMM keeps
            // the host device memory mapped there until the extent is reused.
            // Unless a guest vma still maps it: RM's unmap does not reach guest
            // page tables, and handing the extent to the next mapping would
            // show that process's memory to this one (M-3). Then the release
            // waits for the last MUNMAP.
            self.end_rm_mapping(entry, "UNMAP_MEMORY");
        } else {
            // Host returned RM error — put the entry back
            log::warn!(
                "UNMAP_MEMORY: host RM status 0x{:x}, restoring mapping",
                status
            );
            self.active_maps.insert(entry.region.offset, entry);
        }

        Ok(IoctlOut::ok(param_buf))
    }
}
