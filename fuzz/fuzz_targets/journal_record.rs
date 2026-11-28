#![no_main]
//! `journal::Record::parse`: native journal protocol records sent by guest processes.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = droidc::journal::Record::parse(data);
});
