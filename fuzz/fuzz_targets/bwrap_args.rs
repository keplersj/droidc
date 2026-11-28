#![no_main]
//! `bwrap::parse`: bubblewrap argv as Flatpak and other guest tools build it, with data descriptors.

use libfuzzer_sys::fuzz_target;

struct Fds<'a>(&'a [u8]);

impl droidc::bwrap::FdSource for Fds<'_> {
    fn read_all(&mut self, _fd: i32) -> droidc::error::Result<Vec<u8>> {
        Ok(self.0.to_vec())
    }
    fn path_of(&mut self, fd: i32) -> droidc::error::Result<String> {
        Ok(format!("/fd/{fd}"))
    }
}

fuzz_target!(|input: (Vec<String>, Vec<u8>)| {
    let (argv, fd_data) = input;
    let mut fds = Fds(&fd_data);
    let resolve = |p: &str| if p.is_empty() { None } else { Some(p.to_string()) };
    let _ = droidc::bwrap::parse(&argv, &mut fds, &resolve);
});
