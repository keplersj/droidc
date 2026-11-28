#![no_main]
//! `session::parse_argv` and `parse_services`: JSON argv lists from Selkie and image config.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: &str| {
    let _ = droidc::session::parse_argv(s);
    let _ = droidc::session::parse_services(s);
});
