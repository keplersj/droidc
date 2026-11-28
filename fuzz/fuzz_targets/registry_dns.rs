#![no_main]
//! DNS server lists and resolv.conf, read from the guest and from Android's properties.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|s: &str| {
    let _ = droidc::registry::parse_dns_list(s);
    let _ = droidc::registry::parse_resolv_conf(s);
});
