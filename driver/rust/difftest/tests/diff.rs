//! The C parsers and the Rust core, side by side, on generated scenarios.
//! `DIFFTEST_ITERS` sets how many per family (default 4000);
//! `DIFFTEST_SEED` runs one seed, for a failure's reproduction.

use nvgpu_guest_difftest::scen::{self, Call, Scenario};
use nvgpu_guest_difftest::world::Ev;

fn iters() -> u64 {
    std::env::var("DIFFTEST_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(4000)
}

fn seeds(family: u64) -> Vec<u64> {
    if let Some(s) = std::env::var("DIFFTEST_SEED").ok().and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
        return vec![s];
    }
    (0..iters()).map(|i| family.wrapping_mul(0x1_0000_0000) ^ i.wrapping_mul(0x9e37_79b9_7f4a_7c15)).collect()
}

/// Run every scenario; fail with the first few differences.
fn check(family: &str, gen: impl Fn(u64) -> Scenario, seeds: Vec<u64>) -> Vec<(Scenario, scen::Outcome)> {
    let mut bad = Vec::new();
    let mut ok = Vec::new();
    for s in seeds.into_iter().map(gen) {
        match scen::diff(&s) {
            Ok(o) => ok.push((s, o)),
            Err(e) => bad.push(e),
        }
    }
    if !bad.is_empty() {
        let n = bad.len();
        bad.truncate(5);
        panic!("{family}: {n} scenarios differ; the first:\n{}", bad.join("\n\n"));
    }
    ok
}

#[test]
fn rm_escapes_agree() {
    let ok = check("RM", scen::gen_rm, seeds(1));
    // The generator reached what it is meant to reach.
    let sends = ok.iter().filter(|(_, o)| o.world.events.iter().any(|e| matches!(e, Ev::Send(_)))).count();
    let keeps = ok.iter().filter(|(_, o)| o.world.events.iter().any(|e| matches!(e, Ev::Keep { .. }))).count();
    let warns = ok.iter().filter(|(_, o)| o.world.events.iter().any(|e| matches!(e, Ev::Warn(_)))).count();
    let good = ok.iter().filter(|(_, o)| o.ret == 0).count();
    eprintln!("RM: {} scenarios, {sends} sent something, {good} returned 0, {keeps} kept pins, {warns} warned", ok.len());
    if ok.len() > 1000 {
        assert!(sends > ok.len() / 3 && good > ok.len() / 10 && keeps > 0 && warns > 0);
    }
}

#[test]
fn ioctl2_agrees() {
    let ok = check("IOCTL2", scen::gen_i2, seeds(2));
    let sent = ok.iter().filter(|(_, o)| o.world.events.iter().any(|e| matches!(e, Ev::Send(_)))).count();
    let good = ok.iter().filter(|(_, o)| o.ret == 0).count();
    let outs = ok
        .iter()
        .filter(|(_, o)| o.world.events.iter().any(|e| matches!(e, Ev::Hook(nvgpu_guest_difftest::world::Hook::FdOut { .. }))))
        .count();
    let gems = ok
        .iter()
        .filter(|(_, o)| o.world.events.iter().any(|e| matches!(e, Ev::Hook(nvgpu_guest_difftest::world::Hook::GemOut { .. }))))
        .count();
    let modeset = ok.iter().filter(|(s, o)| matches!(s.call, Call::I2 { sclass: 3, .. }) && o.ret == 0).count();
    eprintln!(
        "IOCTL2: {} scenarios, {sent} sent, {good} returned 0, {outs} made descriptors, {gems} made GEM handles, {modeset} NVKMS succeeded",
        ok.len()
    );
    if ok.len() > 1000 {
        assert!(sent > ok.len() / 4 && good > ok.len() / 20 && outs > 0 && gems > 0);
    }
}

#[test]
fn atomic_agrees() {
    use nvgpu_guest_difftest::world::Hook;
    let ok = check("ATOMIC", scen::gen_atomic, seeds(3));
    let parsed = ok
        .iter()
        .filter(|(_, o)| o.world.events.iter().any(|e| matches!(e, Ev::Hook(Hook::AtomicOut { .. }))))
        .count();
    let has = |f: &dyn Fn(&Hook) -> bool| {
        ok.iter().filter(|(_, o)| o.world.events.iter().any(|e| matches!(e, Ev::Hook(h) if f(h)))).count()
    };
    let reserved = has(&|h| matches!(h, Hook::AReserve { .. }));
    let in_f = has(&|h| matches!(h, Hook::AInFence { .. }));
    let out_f = has(&|h| matches!(h, Hook::AOutFence { .. }));
    let learned = has(&|h| matches!(h, Hook::ALearn { .. }));
    let sent = ok.iter().filter(|(_, o)| o.world.events.iter().any(|e| matches!(e, Ev::Send(_)))).count();
    eprintln!(
        "ATOMIC: {} scenarios, {parsed} parsed, {reserved} reserved events, {in_f} in-fences, {out_f} out-fences, {learned} learned, {sent} sent",
        ok.len()
    );
    if ok.len() > 1000 {
        assert!(parsed > ok.len() / 3 && reserved > 0 && in_f > 0 && out_f > 0 && learned > 0 && sent > 0);
    }
}
