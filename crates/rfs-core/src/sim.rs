//! Deterministic crash-recovery simulation.
//!
//! The lower half of the testing pipeline: torture the engine in user space
//! before it ever runs on hardware. This drives many pseudo-random byte streams
//! through [`crate::testkit::fuzz_crash_recovery`] — the same model-checked
//! driver the `cargo fuzz` target uses — so each seed runs a random mix of
//! insert / delete / `fsync` / commit / power-cut steps and asserts recovery is
//! always the last committed state plus the `fsync`'d ops, never a torn or
//! corrupt in-between.

use crate::testkit::fuzz_crash_recovery;

/// Small deterministic PRNG (LCG) — reproducible, no external crate.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 24
    }
}

#[test]
fn crash_recovery_simulation() {
    for seed in 0..48u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
        let mut buf = [0u8; 512];
        for b in &mut buf {
            *b = (rng.next() & 0xFF) as u8;
        }
        fuzz_crash_recovery(&buf);
    }
}
