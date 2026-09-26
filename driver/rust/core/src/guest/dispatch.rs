//! Which path an ioctl on one of the `/dev/nvidia*` files (or a DRM file's
//! driver range) takes: `nvgpu_ioctl_fd()` and `nvgpu_uvm_ioctl()`.
//!
//! One thing is new here. An RM escape that may be a registration of the
//! caller's memory (ALLOC_MEMORY, RM_ALLOC, VID_HEAP_CONTROL, of exactly that
//! call's size) is copied in once, and the same bytes go to whichever path
//! it takes: the C read the class word once to decide and the whole block
//! again to register, or again to forward, so a racing thread could have one
//! path decided and another's bytes sent.

use super::osdesc;
use super::rm;
use super::wire::{EPERM, EPROTO};

/// `NV_ESC_RM_FREE`.
pub const ESC_RM_FREE: u32 = 0x29;
/// `NV_ESC_RM_CONTROL`.
pub const ESC_RM_CONTROL: u32 = 0x2a;
/// `NV_ESC_RM_ALLOC`.
pub const ESC_RM_ALLOC: u32 = 0x2b;
/// `NV_ESC_RM_IDLE_CHANNELS`.
pub const ESC_RM_IDLE_CHANNELS: u32 = 0x41;
/// `NVGPU_FDT_UVM`: a translation entry naming a UVM command.
pub const FDT_UVM: u32 = 0x8000_0000;

fn ioc_type(cmd: u32) -> u32 {
    (cmd >> 8) & 0xff
}

/// `nvgpu_ioctl_fd()`: an ioctl on one of our files.
pub fn ioctl_fd<E: osdesc::Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64) -> i32 {
    let nr = cmd & 0xff;
    let sz = (cmd >> 16) & 0x3fff;
    let rm_type = ioc_type(cmd) == rm::RM_IOCTL_TYPE;

    // Memory the caller already has, registered by its pages rather than its
    // address, before ALLOC_MEMORY's descriptor translation: RM reads no
    // descriptor for this class. Read once, for whichever path it takes. A
    // block that does not read goes on to the path it would take otherwise,
    // which reads it itself and fails as it does.
    let mut pre = None;
    if rm_type && env.os_desc() && osdesc::candidate(nr, sz) {
        if let Some(mut b) = env.alloc(sz as usize) {
            if env.copy_from_user(b.as_mut(), uarg).is_ok() {
                if let Some(r) = osdesc::ioctl(env, cmd, uarg, b.as_ref()) {
                    return r;
                }
                pre = Some(b);
            }
        }
    }
    let pre = pre.as_ref().map(|b| b.as_ref());

    if rm_type {
        if let Some(po) = env.fd_translation(nr) {
            return match pre {
                Some(d) => rm::translate_fd_bytes(env, cmd, uarg, d, po),
                None => rm::translate_fd(env, cmd, uarg, sz, po),
            };
        }
    }

    match nr {
        ESC_RM_CONTROL => rm::rm_control(env, cmd, uarg, sz),
        ESC_RM_ALLOC => match pre {
            Some(d) => rm::rm_alloc_bytes(env, cmd, uarg, d),
            None => rm::rm_alloc(env, cmd, uarg, sz),
        },
        ESC_RM_IDLE_CHANNELS if rm_type => rm::idle_channels(env, cmd, uarg, sz),
        ESC_RM_FREE => {
            let r = rm::simple(env, cmd, uarg, sz);
            // An object RM freed may have been registered memory.
            if rm_type {
                env.reap();
            }
            r
        }
        _ => match pre {
            Some(d) => rm::simple_bytes(env, cmd, uarg, d),
            None => rm::simple(env, cmd, uarg, sz),
        },
    }
}

/// `nvgpu_uvm_ioctl()`: exactly the command's block, both ways, at the size
/// the host's release gives it; a command naming another of the caller's
/// files through that file's handle.
pub fn uvm_ioctl<E: rm::Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64) -> i32 {
    let Some(sz) = env.uvm_size(cmd) else {
        return -EPERM;
    };
    if cmd & FDT_UVM == 0 {
        if let Some(packed) = env.fd_translation(FDT_UVM | cmd) {
            // The backend states the size too; one that disagrees with the
            // table is a backend of another release, and nothing is sent.
            if packed >> 16 != sz {
                return -EPROTO;
            }
            return rm::translate_fd(env, cmd, uarg, sz, packed & 0xffff);
        }
    }
    rm::simple(env, cmd, uarg, sz)
}
