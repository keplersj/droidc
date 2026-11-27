//! droidc engine library.
/// `0.1.0 (9cf69ff)`: the package version and, when known, the git revision it was built from.
pub const VERSION: &str = env!("DROIDC_VERSION");

/// Short git revision droidc was built from, `-dirty` if the tree had changes; `None` if unknown.
pub fn git_rev() -> Option<&'static str> {
    Some(env!("DROIDC_GIT_REV")).filter(|r| !r.is_empty())
}

pub mod adb;
pub mod apps;
pub mod audio;
pub mod bridge;
pub mod bwrap;
pub mod cgroups;
pub mod deploy;
pub mod displays;
pub mod enter;
pub mod error;
pub mod etcmerge;
pub mod events;
pub mod fsutil;
pub mod host_link;
pub mod journal;
pub mod layout;
pub mod link;
pub mod logcat;
pub mod login1;
pub mod ownership;
pub mod platform;
pub mod polkit;
pub mod record;
pub mod registry;
pub mod session;
pub mod spawn;
pub mod status;
pub mod supervise;
pub mod unpack;
pub mod usersvc;
pub mod verify_glue;
