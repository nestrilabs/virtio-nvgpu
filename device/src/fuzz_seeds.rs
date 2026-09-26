// SPDX-License-Identifier: Apache-2.0
//! Seeds for the `backend` fuzz targets, recorded from the unit tests.
//!
//! Test only, and only with `NVGPU_FUZZ_SEEDS=<dir>` set (`scripts/fuzz.sh
//! seeds`): every message a test serves is kept, and when its backend is
//! dropped the sequence is written to `<dir>` in the input format of
//! `fuzzing/backend.rs` -- a configuration byte saying what the test's
//! backend had (a v2 session, compute, guest RAM, process ids, compositor
//! mode, fences, the driver release), sixteen bytes of calm host, then each
//! message with its capacity. The tests reach every path someone thought
//! worth a test, which a fuzzer starting from nothing takes hours to find.
//!
//! HELLO itself is left out (the harness says it from the configuration
//! byte, before opening its files; a recorded fresh HELLO would close them).

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};

use protocol::messages::{GCAP_PROC_ID, MsgType};

use crate::nvidia::NvidiaBackend;

thread_local! {
    static LOG: RefCell<Vec<(Vec<u8>, usize)>> = const { RefCell::new(Vec::new()) };
    static PROC_IDS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn dir() -> Option<std::path::PathBuf> {
    std::env::var_os("NVGPU_FUZZ_SEEDS").map(Into::into)
}

/// A message `serve` was handed.
pub(crate) fn served(req: &[u8], cap: usize) {
    if dir().is_none() || req.len() > usize::from(u16::MAX) {
        return;
    }
    let ty = req
        .get(..4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
    if ty == Some(MsgType::Hello as u32) {
        let caps = req
            .get(24..28)
            .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
        PROC_IDS.with(|p| p.set(caps & GCAP_PROC_ID != 0));
        return;
    }
    LOG.with(|l| l.borrow_mut().push((req.to_vec(), cap)));
}

/// The backend is going: write what it served.
pub(crate) fn dropped(be: &NvidiaBackend) {
    let Some(dir) = dir() else { return };
    let msgs = LOG.with(|l| std::mem::take(&mut *l.borrow_mut()));
    if msgs.is_empty() {
        return;
    }
    let mut cfg = 0u8;
    if be.session.v2 {
        cfg |= 1;
    }
    if be.config.allow_compute {
        cfg |= 2;
    }
    if be.guest_ram.is_some() {
        cfg |= 4;
    }
    if PROC_IDS.with(|p| p.replace(false)) {
        cfg |= 8;
    }
    if be.config.kms_card {
        cfg |= 16;
    }
    if be.config.fences {
        cfg |= 32;
    }
    let release = be.driver.map(|v| v.to_string());
    cfg |= match release.as_deref() {
        Some("535.129.03") => 1,
        Some("595.99.02") => 2,
        Some(_) => 3,
        None => 0,
    } << 6;
    let mut out = vec![cfg];
    out.extend_from_slice(&[0u8; 16]);
    for (m, cap) in msgs {
        out.extend_from_slice(&(m.len() as u16).to_le_bytes());
        out.extend_from_slice(&m);
        out.extend_from_slice(&(cap.min(usize::from(u16::MAX)) as u16).to_le_bytes());
    }
    static N: AtomicUsize = AtomicUsize::new(0);
    let name = std::thread::current()
        .name()
        .unwrap_or("test")
        .replace("::", "-");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(
        dir.join(format!("{name}-{}", N.fetch_add(1, Ordering::Relaxed))),
        out,
    );
}
