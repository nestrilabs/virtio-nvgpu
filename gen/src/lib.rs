// crates/abi/src/lib.rs
//
// NVIDIA kernel driver ABI definitions.
//
// Ported from gVisor's pkg/abi/nvgpu/ (Apache-2.0).

pub mod fixtures;
pub mod ioctl;
pub mod osdesc;
pub mod rmallow;
pub mod rmctrl;
pub mod types;
pub mod uvm;
pub mod version;
pub mod versions;
pub mod vidmem;
