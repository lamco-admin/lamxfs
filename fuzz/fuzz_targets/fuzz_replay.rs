//! Fuzz log replay on a synthetic volume: no panic, no OOB, no hang.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    lamxfs::fuzz_replay(data);
});
