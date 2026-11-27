//! A stand-in for NetworkManager's connectivity on the container's system bus, fed with Android's
//! network state.
//!
//! Android owns the device's networks; the guest only borrows its connection. Without NetworkManager,
//! GLib's network monitor (`GNetworkMonitor`, which apps, Flatpak and the NetworkMonitor portal use)
//! falls back to reading routes, so it can't tell a captive portal or a network without internet from
//! a working one, and never says a connection is metered. `droidc host-link` serves the
//! `org.freedesktop.NetworkManager` manager object with the properties that monitor reads
//! (`Connectivity`, `Metered`, `State`, `NetworkingEnabled`, `PrimaryConnectionType`), and the host's
//! `network` lines (`ConnectivityManager`'s default network) update it. So an app that waits for a
//! connection, or a store that holds updates on a metered one, follows Android.
//!
//! No devices, connections or settings are served: those stay Android's, in its Quick Settings. With
//! no object manager either, libnm (gnome-shell's network menu, `nmcli`) sees no NetworkManager running,
//! as before. A real NetworkManager that owns the name is left in charge.

use crate::error::{Error, Result};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use zbus::zvariant::OwnedObjectPath;

pub const NAME: &str = "org.freedesktop.NetworkManager";
pub const PATH: &str = "/org/freedesktop/NetworkManager";

/// Android's default network, as the host reads it from `ConnectivityManager`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Network {
    /// There is a default network.
    pub connected: Option<bool>,
    /// It reaches the internet (`NET_CAPABILITY_VALIDATED`).
    pub validated: Option<bool>,
    /// It sits behind a sign-in page (`NET_CAPABILITY_CAPTIVE_PORTAL`).
    pub portal: Option<bool>,
    /// It is metered (no `NET_CAPABILITY_NOT_METERED`).
    pub metered: Option<bool>,
    /// `wifi`, `cellular`, `ethernet`, `bluetooth` or `vpn`.
    pub transport: Option<String>,
}

// NetworkManager's enums (nm-dbus-interface.h).
const STATE_DISCONNECTED: u32 = 20;
const STATE_CONNECTED_SITE: u32 = 60;
const STATE_CONNECTED_GLOBAL: u32 = 70;
const CONNECTIVITY_NONE: u32 = 1;
const CONNECTIVITY_PORTAL: u32 = 2;
const CONNECTIVITY_LIMITED: u32 = 3;
const CONNECTIVITY_FULL: u32 = 4;
const METERED_UNKNOWN: u32 = 0;
const METERED_YES: u32 = 1;
const METERED_NO: u32 = 2;

/// What the manager reports for a [`Network`].
#[derive(Debug, Clone, PartialEq)]
pub struct ManagerState {
    pub state: u32,
    pub connectivity: u32,
    pub metered: u32,
    pub primary_type: String,
}

impl Default for ManagerState {
    /// Before the host's first line: connected, as the route-reading monitor would say.
    fn default() -> Self {
        ManagerState {
            state: STATE_CONNECTED_GLOBAL,
            connectivity: CONNECTIVITY_FULL,
            metered: METERED_UNKNOWN,
            primary_type: String::new(),
        }
    }
}

impl ManagerState {
    pub fn from_network(n: &Network) -> ManagerState {
        let connected = n.connected.unwrap_or(true);
        let (state, connectivity) = if !connected {
            (STATE_DISCONNECTED, CONNECTIVITY_NONE)
        } else if n.portal == Some(true) {
            (STATE_CONNECTED_SITE, CONNECTIVITY_PORTAL)
        } else if n.validated == Some(false) {
            (STATE_CONNECTED_SITE, CONNECTIVITY_LIMITED)
        } else {
            (STATE_CONNECTED_GLOBAL, CONNECTIVITY_FULL)
        };
        let metered = match n.metered {
            _ if !connected => METERED_UNKNOWN,
            Some(true) => METERED_YES,
            Some(false) => METERED_NO,
            None => METERED_UNKNOWN,
        };
        let primary_type = match n.transport.as_deref() {
            _ if !connected => "",
            Some("wifi") => "802-11-wireless",
            Some("cellular") => "gsm",
            Some("ethernet") => "802-3-ethernet",
            Some("bluetooth") => "bluetooth",
            Some("vpn") => "vpn",
            _ => "",
        }
        .to_string();
        ManagerState {
            state,
            connectivity,
            metered,
            primary_type,
        }
    }
}

type Shared = Arc<Mutex<ManagerState>>;

fn read(s: &Shared) -> ManagerState {
    s.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn no_path() -> OwnedObjectPath {
    OwnedObjectPath::try_from("/").unwrap()
}

struct Manager {
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.NetworkManager")]
impl Manager {
    fn get_devices(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    fn get_all_devices(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    fn get_permissions(&self) -> std::collections::HashMap<String, String> {
        Default::default()
    }

    /// Android checks connectivity itself; this reports what it last said.
    fn check_connectivity(&self) -> u32 {
        read(&self.state).connectivity
    }

    #[zbus(name = "state")]
    fn get_state(&self) -> u32 {
        read(&self.state).state
    }

    /// NetworkManager's own `StateChanged` signal, besides the property's.
    #[zbus(signal, name = "StateChanged")]
    async fn state_signal(
        e: &zbus::object_server::SignalEmitter<'_>,
        state: u32,
    ) -> zbus::Result<()>;

    #[zbus(property)]
    fn devices(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    #[zbus(property)]
    fn all_devices(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    #[zbus(property)]
    fn active_connections(&self) -> Vec<OwnedObjectPath> {
        Vec::new()
    }

    #[zbus(property)]
    fn primary_connection(&self) -> OwnedObjectPath {
        no_path()
    }

    #[zbus(property)]
    fn activating_connection(&self) -> OwnedObjectPath {
        no_path()
    }

    #[zbus(property)]
    fn primary_connection_type(&self) -> String {
        read(&self.state).primary_type
    }

    #[zbus(property)]
    fn metered(&self) -> u32 {
        read(&self.state).metered
    }

    #[zbus(property)]
    fn networking_enabled(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn wireless_enabled(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn wireless_hardware_enabled(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn wwan_enabled(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn wwan_hardware_enabled(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn startup(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn version(&self) -> String {
        format!("droidc-{}", crate::VERSION)
    }

    #[zbus(property)]
    fn state(&self) -> u32 {
        read(&self.state).state
    }

    #[zbus(property)]
    fn connectivity(&self) -> u32 {
        read(&self.state).connectivity
    }

    #[zbus(property)]
    fn connectivity_check_available(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn connectivity_check_enabled(&self) -> bool {
        true
    }
}

/// The served manager. Keep it to keep serving.
pub struct Server {
    conn: zbus::blocking::Connection,
    state: Shared,
}

impl Server {
    /// Serves on `conn` (the container's system bus). Fails if a real NetworkManager owns the name.
    pub fn serve(conn: zbus::blocking::Connection) -> Result<Server> {
        let ctx = |e: zbus::Error| Error::internal(format!("system bus: {e}"));
        let state: Shared = Arc::new(Mutex::new(ManagerState::default()));
        conn.object_server()
            .at(
                PATH,
                Manager {
                    state: state.clone(),
                },
            )
            .map_err(ctx)?;
        use zbus::fdo::RequestNameFlags as F;
        match conn.request_name_with_flags(NAME, F::DoNotQueue.into()) {
            Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => {}
            Ok(_) => {
                return Err(Error::new(
                    crate::error::ErrorKind::Locked,
                    format!("{NAME} is owned by another service"),
                ))
            }
            Err(e) => return Err(ctx(e)),
        }
        Ok(Server { conn, state })
    }

    /// Takes the host's network state, announcing what changed.
    pub fn set(&self, n: &Network) -> Result<()> {
        let new = ManagerState::from_network(n);
        let old = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *s, new.clone())
        };
        if old == new {
            return Ok(());
        }
        let ctx = |e: zbus::Error| Error::internal(format!("system bus: {e}"));
        let m = self
            .conn
            .object_server()
            .interface::<_, Manager>(PATH)
            .map_err(ctx)?;
        let (g, e) = (m.get(), m.signal_emitter());
        zbus::block_on(async {
            if old.state != new.state {
                g.state_changed(e).await?;
                Manager::state_signal(e, new.state).await?;
            }
            if old.connectivity != new.connectivity {
                g.connectivity_changed(e).await?;
            }
            if old.metered != new.metered {
                g.metered_changed(e).await?;
            }
            if old.primary_type != new.primary_type {
                g.primary_connection_type_changed(e).await?;
            }
            Ok::<_, zbus::Error>(())
        })
        .map_err(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    fn net(validated: bool, metered: bool, transport: &str) -> Network {
        Network {
            connected: Some(true),
            validated: Some(validated),
            portal: Some(false),
            metered: Some(metered),
            transport: Some(transport.into()),
        }
    }

    #[test]
    fn android_networks_map_to_networkmanager_states() {
        let s = ManagerState::from_network(&net(true, false, "wifi"));
        assert_eq!(
            (s.state, s.connectivity, s.metered, s.primary_type.as_str()),
            (70, 4, 2, "802-11-wireless")
        );
        let s = ManagerState::from_network(&net(true, true, "cellular"));
        assert_eq!(
            (s.connectivity, s.metered, s.primary_type.as_str()),
            (4, 1, "gsm")
        );
        // Connected, but no internet: limited.
        let s = ManagerState::from_network(&net(false, false, "wifi"));
        assert_eq!((s.state, s.connectivity), (60, 3));
        // A sign-in page.
        let mut n = net(false, false, "wifi");
        n.portal = Some(true);
        let s = ManagerState::from_network(&n);
        assert_eq!((s.state, s.connectivity), (60, 2));
        // No network.
        let s = ManagerState::from_network(&Network {
            connected: Some(false),
            metered: Some(true),
            transport: Some("wifi".into()),
            ..Default::default()
        });
        assert_eq!(
            (s.state, s.connectivity, s.metered, s.primary_type.as_str()),
            (20, 1, 0, "")
        );
        // Only `connected` known: assume it works.
        let s = ManagerState::from_network(&Network {
            connected: Some(true),
            ..Default::default()
        });
        assert_eq!((s.state, s.connectivity, s.metered), (70, 4, 0));
    }

    fn private_bus() -> Option<(std::process::Child, String)> {
        let mut daemon = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let mut addr = String::new();
        std::io::BufReader::new(daemon.stdout.take()?)
            .read_line(&mut addr)
            .ok()?;
        Some((daemon, addr.trim().to_string()))
    }

    fn get(conn: &zbus::blocking::Connection, prop: &str) -> zbus::zvariant::OwnedValue {
        let r = conn
            .call_method(
                Some(NAME),
                PATH,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(NAME, prop),
            )
            .unwrap();
        r.body().deserialize().unwrap()
    }

    /// On a private bus: the manager's properties follow the host, and a second one is refused.
    #[test]
    fn serves_connectivity_on_a_bus() {
        let Some((mut daemon, addr)) = private_bus() else {
            eprintln!("skipped: no dbus-daemon");
            return;
        };
        let connect = || {
            zbus::blocking::connection::Builder::address(addr.as_str())
                .unwrap()
                .build()
                .unwrap()
        };
        let server = Server::serve(connect()).unwrap();
        let client = connect();
        assert_eq!(u32::try_from(get(&client, "Connectivity")).unwrap(), 4);

        server.set(&net(true, true, "cellular")).unwrap();
        assert_eq!(u32::try_from(get(&client, "Metered")).unwrap(), 1);
        assert_eq!(
            String::try_from(get(&client, "PrimaryConnectionType")).unwrap(),
            "gsm"
        );
        server
            .set(&Network {
                connected: Some(false),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(u32::try_from(get(&client, "Connectivity")).unwrap(), 1);
        assert_eq!(u32::try_from(get(&client, "State")).unwrap(), 20);
        let r = client
            .call_method(Some(NAME), PATH, Some(NAME), "state", &())
            .unwrap();
        assert_eq!(r.body().deserialize::<u32>().unwrap(), 20);

        assert!(Server::serve(connect()).is_err(), "the name is taken");
        drop(server);
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
}
