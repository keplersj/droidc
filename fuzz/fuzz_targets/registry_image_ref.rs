#![no_main]
//! `ImageRef::parse`: image references come from the command line, config and other images.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: &str| {
    let _ = droidc::registry::ImageRef::parse(s);
});
