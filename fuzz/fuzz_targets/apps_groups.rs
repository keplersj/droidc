#![no_main]
//! `apps::parse_groups`: desktop entries shipped by images and installed apps.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: &str| {
    let _ = droidc::apps::parse_groups(s);
});
