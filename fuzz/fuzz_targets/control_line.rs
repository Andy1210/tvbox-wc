//! The control socket's decoding: framing, the line cap, request decoding,
//! argument checks and reply encoding. Run with `cargo +nightly fuzz run control_line`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    tvbox_wc::fuzz::control_stream(data);
});
