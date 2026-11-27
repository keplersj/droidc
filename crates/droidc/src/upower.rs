//! A stand-in for `upowerd` on the container's system bus, fed with Android's battery state.
//!
//! There is no upowerd in the guest (no udev, no `/sys/class/power_supply` worth reading), so GNOME's
//! top bar shows no battery and gnome-settings-daemon can't warn when it runs low. `droidc host-link`
//! serves `org.freedesktop.UPower` with one battery, and the host's `battery` lines
//! (`BatteryManager`'s level, status and plug) update it. The display device and the battery carry the
//! same values, as upowerd's do with a single battery.
//!
//! What is served is what gnome-shell's power indicator and gsd-power read: the manager's
//! `EnumerateDevices`, `GetDisplayDevice`, `GetCriticalAction` and `OnBattery`, and each device's
//! `Type`, `PowerSupply`, `IsPresent`, `State`, `Percentage`, `TimeToEmpty`, `TimeToFull`,
//! `WarningLevel` and `IconName`. History and statistics are empty. A real upowerd that owns the name
//! is left in charge.

use crate::error::{Error, Result};
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use zbus::zvariant::OwnedObjectPath;

pub const NAME: &str = "org.freedesktop.UPower";
pub const PATH: &str = "/org/freedesktop/UPower";
pub const DISPLAY_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
pub const BATTERY_PATH: &str = "/org/freedesktop/UPower/devices/battery_BAT0";

/// Android's battery, as the host reads it from `BatteryManager` / `ACTION_BATTERY_CHANGED`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Battery {
    /// The device has a battery (`EXTRA_PRESENT`); false on a TV, say.
    pub present: Option<bool>,
    /// Level in percent.
    pub percent: Option<f64>,
    /// `charging`, `discharging`, `full` or `not-charging` (`EXTRA_STATUS`).
    pub status: Option<String>,
    /// Plugged in to any power source (`EXTRA_PLUGGED` non-zero).
    pub plugged: Option<bool>,
    /// Seconds until full while charging (`BatteryManager.computeChargeTimeRemaining`), if known.
    pub time_to_full: Option<i64>,
}

// UPower's enums (up-types.h).
const TYPE_BATTERY: u32 = 2;
const STATE_UNKNOWN: u32 = 0;
const STATE_CHARGING: u32 = 1;
const STATE_DISCHARGING: u32 = 2;
const STATE_FULLY_CHARGED: u32 = 4;
const STATE_PENDING_CHARGE: u32 = 5;
const WARNING_NONE: u32 = 1;
const WARNING_LOW: u32 = 3;
const WARNING_CRITICAL: u32 = 4;
const LEVEL_NONE: u32 = 1;
const TECHNOLOGY_LITHIUM_ION: u32 = 1;

/// What a UPower device reports for `b`.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceState {
    pub is_present: bool,
    pub state: u32,
    pub percentage: f64,
    pub on_battery: bool,
    pub warning_level: u32,
    pub icon_name: String,
    pub time_to_full: i64,
}

impl DeviceState {
    pub fn from_battery(b: &Battery) -> DeviceState {
        let is_present = b.present.unwrap_or(b.percent.is_some());
        let percentage = b
            .percent
            .filter(|p| p.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, 100.0);
        if !is_present {
            return DeviceState {
                is_present: false,
                state: STATE_UNKNOWN,
                percentage: 0.0,
                on_battery: false,
                warning_level: WARNING_NONE,
                icon_name: String::new(),
                time_to_full: 0,
            };
        }
        let plugged = b.plugged.unwrap_or(false);
        let state = match b.status.as_deref() {
            Some("charging") => STATE_CHARGING,
            Some("full") => STATE_FULLY_CHARGED,
            Some("not-charging") => STATE_PENDING_CHARGE,
            Some("discharging") => STATE_DISCHARGING,
            _ if plugged => STATE_PENDING_CHARGE,
            _ => STATE_DISCHARGING,
        };
        let on_battery = !plugged && state == STATE_DISCHARGING;
        let warning_level = if !on_battery {
            WARNING_NONE
        } else if percentage <= 5.0 {
            WARNING_CRITICAL
        } else if percentage <= 10.0 {
            WARNING_LOW
        } else {
            WARNING_NONE
        };
        let level = ((percentage / 10.0).round() as u32 * 10).min(100);
        let icon_name = match state {
            STATE_FULLY_CHARGED => "battery-level-100-charged-symbolic".to_string(),
            STATE_CHARGING | STATE_PENDING_CHARGE => {
                format!("battery-level-{level}-charging-symbolic")
            }
            _ => format!("battery-level-{level}-symbolic"),
        };
        DeviceState {
            is_present,
            state,
            percentage,
            on_battery,
            warning_level,
            icon_name,
            time_to_full: if state == STATE_CHARGING {
                b.time_to_full.filter(|t| *t > 0).unwrap_or(0)
            } else {
                0
            },
        }
    }
}

type Shared = Arc<Mutex<DeviceState>>;

fn read(s: &Shared) -> DeviceState {
    s.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

struct Manager {
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.UPower")]
impl Manager {
    fn enumerate_devices(&self) -> Vec<OwnedObjectPath> {
        vec![OwnedObjectPath::try_from(BATTERY_PATH).unwrap()]
    }
    fn get_display_device(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(DISPLAY_PATH).unwrap()
    }
    /// What gsd-power would do at a critical level; it can't, here (login1 refuses power operations).
    fn get_critical_action(&self) -> String {
        "PowerOff".into()
    }
    #[zbus(property)]
    fn daemon_version(&self) -> String {
        format!("droidc-{}", crate::VERSION)
    }
    #[zbus(property)]
    fn on_battery(&self) -> bool {
        read(&self.state).on_battery
    }
    #[zbus(property)]
    fn lid_is_closed(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn lid_is_present(&self) -> bool {
        false
    }
}

struct Device {
    state: Shared,
}

#[zbus::interface(name = "org.freedesktop.UPower.Device")]
impl Device {
    fn refresh(&self) {}
    fn get_history(&self, _type: &str, _timespan: u32, _resolution: u32) -> Vec<(u32, f64, u32)> {
        Vec::new()
    }
    fn get_statistics(&self, _type: &str) -> Vec<(f64, f64)> {
        Vec::new()
    }
    #[zbus(property)]
    fn native_path(&self) -> String {
        "android".into()
    }
    #[zbus(property)]
    fn vendor(&self) -> String {
        String::new()
    }
    #[zbus(property)]
    fn model(&self) -> String {
        "Android battery".into()
    }
    #[zbus(property, name = "Type")]
    fn kind(&self) -> u32 {
        TYPE_BATTERY
    }
    #[zbus(property)]
    fn power_supply(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn is_present(&self) -> bool {
        read(&self.state).is_present
    }
    #[zbus(property)]
    fn is_rechargeable(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn state(&self) -> u32 {
        read(&self.state).state
    }
    #[zbus(property)]
    fn percentage(&self) -> f64 {
        read(&self.state).percentage
    }
    #[zbus(property)]
    fn time_to_empty(&self) -> i64 {
        0
    }
    #[zbus(property)]
    fn time_to_full(&self) -> i64 {
        read(&self.state).time_to_full
    }
    #[zbus(property)]
    fn warning_level(&self) -> u32 {
        read(&self.state).warning_level
    }
    #[zbus(property)]
    fn battery_level(&self) -> u32 {
        LEVEL_NONE
    }
    #[zbus(property)]
    fn icon_name(&self) -> String {
        read(&self.state).icon_name
    }
    #[zbus(property)]
    fn technology(&self) -> u32 {
        TECHNOLOGY_LITHIUM_ION
    }
}

/// The served UPower. Keep it to keep serving.
pub struct Server {
    conn: zbus::blocking::Connection,
    state: Shared,
}

impl Server {
    /// Serves on `conn` (the container's system bus). Fails if a real upowerd owns the name.
    pub fn serve(conn: zbus::blocking::Connection) -> Result<Server> {
        let ctx = |e: zbus::Error| Error::internal(format!("system bus: {e}"));
        let state: Shared = Arc::new(Mutex::new(DeviceState::from_battery(&Battery::default())));
        let os = conn.object_server();
        os.at(
            PATH,
            Manager {
                state: state.clone(),
            },
        )
        .map_err(ctx)?;
        os.at(
            DISPLAY_PATH,
            Device {
                state: state.clone(),
            },
        )
        .map_err(ctx)?;
        os.at(
            BATTERY_PATH,
            Device {
                state: state.clone(),
            },
        )
        .map_err(ctx)?;
        drop(os);
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

    /// Takes the host's battery state, announcing what changed.
    pub fn set(&self, b: &Battery) -> Result<()> {
        let new = DeviceState::from_battery(b);
        let old = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *s, new.clone())
        };
        if old == new {
            return Ok(());
        }
        let ctx = |e: zbus::Error| Error::internal(format!("system bus: {e}"));
        let os = self.conn.object_server();
        if old.on_battery != new.on_battery {
            let m = os.interface::<_, Manager>(PATH).map_err(ctx)?;
            zbus::block_on(m.get().on_battery_changed(m.signal_emitter())).map_err(ctx)?;
        }
        for path in [DISPLAY_PATH, BATTERY_PATH] {
            let d = os.interface::<_, Device>(path).map_err(ctx)?;
            let (g, e) = (d.get(), d.signal_emitter());
            zbus::block_on(async {
                if old.is_present != new.is_present {
                    g.is_present_changed(e).await?;
                }
                if old.state != new.state {
                    g.state_changed(e).await?;
                }
                if old.percentage != new.percentage {
                    g.percentage_changed(e).await?;
                }
                if old.time_to_full != new.time_to_full {
                    g.time_to_full_changed(e).await?;
                }
                if old.warning_level != new.warning_level {
                    g.warning_level_changed(e).await?;
                }
                if old.icon_name != new.icon_name {
                    g.icon_name_changed(e).await?;
                }
                Ok::<_, zbus::Error>(())
            })
            .map_err(ctx)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    fn bat(percent: f64, status: &str, plugged: bool) -> Battery {
        Battery {
            present: Some(true),
            percent: Some(percent),
            status: Some(status.into()),
            plugged: Some(plugged),
            time_to_full: Some(1800),
        }
    }

    #[test]
    fn android_states_map_to_upower() {
        let d = DeviceState::from_battery(&bat(73.0, "discharging", false));
        assert_eq!(
            (d.state, d.on_battery, d.warning_level),
            (STATE_DISCHARGING, true, WARNING_NONE)
        );
        assert_eq!(d.icon_name, "battery-level-70-symbolic");
        assert_eq!(d.time_to_full, 0);

        let d = DeviceState::from_battery(&bat(46.0, "charging", true));
        assert_eq!((d.state, d.on_battery), (STATE_CHARGING, false));
        assert_eq!(d.icon_name, "battery-level-50-charging-symbolic");
        assert_eq!(d.time_to_full, 1800);

        let d = DeviceState::from_battery(&bat(100.0, "full", true));
        assert_eq!(d.state, STATE_FULLY_CHARGED);
        assert_eq!(d.icon_name, "battery-level-100-charged-symbolic");

        // Plugged in but held (charging limits): pending, not on battery.
        let d = DeviceState::from_battery(&bat(80.0, "not-charging", true));
        assert_eq!((d.state, d.on_battery), (STATE_PENDING_CHARGE, false));

        assert_eq!(
            DeviceState::from_battery(&bat(9.0, "discharging", false)).warning_level,
            WARNING_LOW
        );
        assert_eq!(
            DeviceState::from_battery(&bat(4.0, "discharging", false)).warning_level,
            WARNING_CRITICAL
        );
        // Low but charging: no warning.
        assert_eq!(
            DeviceState::from_battery(&bat(4.0, "charging", true)).warning_level,
            WARNING_NONE
        );

        let none = DeviceState::from_battery(&Battery {
            present: Some(false),
            ..Default::default()
        });
        assert!(!none.is_present && !none.on_battery);
        assert!(!DeviceState::from_battery(&Battery::default()).is_present);
    }

    #[test]
    fn serves_the_battery_on_a_bus() {
        let Ok(mut daemon) = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        else {
            eprintln!("skipped: no dbus-daemon");
            return;
        };
        let mut addr = String::new();
        std::io::BufReader::new(daemon.stdout.take().unwrap())
            .read_line(&mut addr)
            .unwrap();
        let conn = |a: &str| {
            zbus::blocking::connection::Builder::address(a)
                .unwrap()
                .build()
                .unwrap()
        };
        let server = Server::serve(conn(addr.trim())).unwrap();
        // A second one sees the name taken and leaves it.
        assert!(Server::serve(conn(addr.trim())).is_err());

        let client = conn(addr.trim());
        let get = |path: &str, iface: &str, prop: &str| -> zbus::zvariant::OwnedValue {
            client
                .call_method(
                    Some(NAME),
                    path,
                    Some("org.freedesktop.DBus.Properties"),
                    "Get",
                    &(iface, prop),
                )
                .unwrap()
                .body()
                .deserialize()
                .unwrap()
        };
        let dev = "org.freedesktop.UPower.Device";
        assert!(!bool::try_from(get(DISPLAY_PATH, dev, "IsPresent")).unwrap());

        server.set(&bat(42.0, "discharging", false)).unwrap();
        assert_eq!(
            f64::try_from(get(DISPLAY_PATH, dev, "Percentage")).unwrap(),
            42.0
        );
        assert_eq!(
            u32::try_from(get(BATTERY_PATH, dev, "Type")).unwrap(),
            TYPE_BATTERY
        );
        assert_eq!(
            u32::try_from(get(BATTERY_PATH, dev, "State")).unwrap(),
            STATE_DISCHARGING
        );
        assert!(bool::try_from(get(PATH, NAME, "OnBattery")).unwrap());
        let icon: String = get(DISPLAY_PATH, dev, "IconName").try_into().unwrap();
        assert_eq!(icon, "battery-level-40-symbolic");

        let devices: Vec<OwnedObjectPath> = client
            .call_method(Some(NAME), PATH, Some(NAME), "EnumerateDevices", &())
            .unwrap()
            .body()
            .deserialize()
            .unwrap();
        assert_eq!(
            devices,
            vec![OwnedObjectPath::try_from(BATTERY_PATH).unwrap()]
        );

        let _ = daemon.kill();
        let _ = daemon.wait();
    }
}
