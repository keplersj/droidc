#![no_main]
//! `bwrap::Launcher::parse`: the proot launcher argv droidc rebuilds sandboxes from.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|argv: Vec<String>| {
    let _ = droidc::bwrap::Launcher::parse(&argv);
});
