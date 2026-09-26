//! Support code for `m5_6_fetch_benchmark.rs`: the latency relay and the
//! process sampling. Kept out of `common/` so that no other test binary
//! compiles it.

#![allow(dead_code, reason = "the benchmark uses a subset in each mode")]
#![allow(
    unreachable_pub,
    reason = "a private `mod` inside one test crate; `pub` is for the parent file"
)]

pub mod relay;
pub mod sample;
