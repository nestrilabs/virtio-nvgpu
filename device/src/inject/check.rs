// SPDX-License-Identifier: Apache-2.0
//! What an IMPORT may describe: the formats capture produces, and the rule
//! that the object holds every plane its layout says, checked in u64.

#![forbid(unsafe_code)]

use protocol::inject::{INJ_F_ALL, InjImport};

use super::MAX_DIM;

/// `DRM_FORMAT_MOD_INVALID`.
pub(super) const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// A DRM format this accepts: planes, bytes per pixel per plane, and the
/// chroma subsampling of the planes after the first.
#[derive(Clone, Copy, Debug)]
struct Format {
    fourcc: u32,
    planes: usize,
    cpp: [u64; 2],
    sub: u64,
}

pub(super) const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    a as u32 | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
}

/// What screen capture produces: 8- and 10-bit RGB in both orders, 16-bit
/// float, RGB565, and the two 4:2:0 YUV layouts an encoder-side portal
/// might offer.
const FORMATS: &[Format] = &[
    Format {
        fourcc: fourcc(b'X', b'R', b'2', b'4'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'A', b'R', b'2', b'4'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'X', b'B', b'2', b'4'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'A', b'B', b'2', b'4'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'X', b'R', b'3', b'0'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'A', b'R', b'3', b'0'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'X', b'B', b'3', b'0'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'A', b'B', b'3', b'0'),
        planes: 1,
        cpp: [4, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'X', b'B', b'4', b'H'),
        planes: 1,
        cpp: [8, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'A', b'B', b'4', b'H'),
        planes: 1,
        cpp: [8, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'R', b'G', b'1', b'6'),
        planes: 1,
        cpp: [2, 0],
        sub: 1,
    },
    Format {
        fourcc: fourcc(b'N', b'V', b'1', b'2'),
        planes: 2,
        cpp: [1, 2],
        sub: 2,
    },
    Format {
        fourcc: fourcc(b'P', b'0', b'1', b'0'),
        planes: 2,
        cpp: [2, 4],
        sub: 2,
    },
];

fn format(fourcc: u32) -> Option<&'static Format> {
    FORMATS.iter().find(|f| f.fourcc == fourcc)
}

/// The IMPORT's own fields, before any descriptor is looked at: EINVAL for
/// anything this does not take.
pub fn check_request(imp: &InjImport) -> Result<(), i32> {
    let f = format(imp.fourcc).ok_or(libc::EINVAL)?;
    let n = imp.nplanes as usize;
    if n != f.planes
        || imp.width == 0
        || imp.height == 0
        || imp.width > MAX_DIM
        || imp.height > MAX_DIM
        || imp.flags & !INJ_F_ALL != 0
        || imp.modifier == MOD_INVALID
    {
        return Err(libc::EINVAL);
    }
    // Planes past nplanes say nothing.
    if imp.offsets[n..]
        .iter()
        .chain(&imp.strides[n..])
        .any(|&v| v != 0)
    {
        return Err(libc::EINVAL);
    }
    Ok(())
}

/// Whether an object of `size` bytes holds every plane `imp` describes:
/// each plane's stride holds its row, and its offset plus stride times its
/// rows lies within the object. u64 throughout, every step checked.
pub fn check_layout(imp: &InjImport, size: u64) -> Result<(), i32> {
    check_request(imp)?;
    let f = format(imp.fourcc).ok_or(libc::EINVAL)?;
    for p in 0..f.planes {
        let sub = if p == 0 { 1 } else { f.sub };
        let w = u64::from(imp.width).div_ceil(sub);
        let h = u64::from(imp.height).div_ceil(sub);
        let row = w.checked_mul(f.cpp[p]).ok_or(libc::EINVAL)?;
        let stride = u64::from(imp.strides[p]);
        if stride < row {
            return Err(libc::EINVAL);
        }
        let end = stride
            .checked_mul(h)
            .and_then(|b| b.checked_add(u64::from(imp.offsets[p])))
            .ok_or(libc::EINVAL)?;
        if end > size {
            return Err(libc::EINVAL);
        }
    }
    Ok(())
}

/// Two tokens equal, in time that does not depend on where they differ.
pub(super) fn token_eq(a: &[u8; 16], b: &[u8; 16]) -> bool {
    let diff = a
        .iter()
        .zip(b)
        .fold(0u8, |acc, (x, y)| acc | std::hint::black_box(x ^ y));
    std::hint::black_box(diff) == 0
}
