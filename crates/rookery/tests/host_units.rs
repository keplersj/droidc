//! Loads every unit the machine running the tests has, if it has systemd units at all. A real distro's
//! units are the best check that the parser and loader accept what images ship.

use std::path::Path;

use rookery::loader::LoadState;
use rookery::paths::UserDirs;
use rookery::{options, Loader, Scope, SearchPath};

#[test]
fn loads_every_host_system_unit() {
    if !Path::new("/usr/lib/systemd/system").is_dir() {
        eprintln!("no /usr/lib/systemd/system here, skipping");
        return;
    }
    let loader = Loader::new(SearchPath::new(Scope::System, &UserDirs::default(), None));
    let mut loaded = 0;
    for name in loader.unit_file_names() {
        let unit = loader
            .load_str(&name)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            matches!(
                unit.state,
                LoadState::Loaded | LoadState::Masked | LoadState::NotFound
            ),
            "{name}: {:?}",
            unit.state
        );
        if unit.state == LoadState::Loaded {
            loaded += 1;
            // Notices must be producible for anything a distro ships.
            let _ = options::notices(&unit);
        }
    }
    eprintln!("loaded {loaded} host units");
}
