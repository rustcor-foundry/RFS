//! Coverage-guided crash-recovery fuzzing.
//!
//! libFuzzer feeds arbitrary byte strings; each decodes to a sequence of
//! insert / delete / fsync / commit / power-cut steps driven against a reference
//! model. Any divergence from the durability guarantee (last commit + fsync'd
//! ops, never a torn/corrupt state) panics and is reported as a crash.
//!
//! The whole driver lives in `rfs_core::testkit` so it is shared with — and
//! verified by — the in-crate deterministic simulation test.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rfs_core::testkit::fuzz_crash_recovery(data);
});
