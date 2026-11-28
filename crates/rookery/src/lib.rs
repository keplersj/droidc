//! Rookery, droidc's systemd stand-in (`docs/design/systemd-standin.md`).
//!
//! This crate is the part of the stand-in that has nothing to do with Android: it reads an image's
//! own unit files the way systemd documents (`systemd.syntax(7)`, `systemd.unit(5)`), so the manager
//! built on it runs what the distro describes instead of a list hard-wired into droidc.
//!
//! Everything here is written from systemd's manual pages and D-Bus documentation, not from systemd's
//! source, so the crate stays MIT like the rest of droidc.
//!
//! This first part is the unit loader:
//!
//! - [`syntax`]: the INI-like file format.
//! - [`name`]: unit names, templates, instances and `systemd-escape` rules.
//! - [`paths`]: the unit search path for the system and user instances, with droidc's own units.
//! - [`loader`]: finding a unit's fragment, aliases, masks, drop-ins and `.wants/` directories.
//! - [`specifier`]: `%n`, `%i` and the other specifiers.
//! - [`condition`]: `Condition*=` and `Assert*=`.
//! - [`options`]: which settings the stand-in honours, and a notice for every one it doesn't.

pub mod condition;
pub mod loader;
pub mod name;
pub mod options;
pub mod paths;
pub mod specifier;
pub mod syntax;

pub use loader::{LoadState, LoadedUnit, Loader};
pub use name::{UnitName, UnitType};
pub use options::Notice;
pub use paths::{Scope, SearchPath};
