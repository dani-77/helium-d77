//! Battery status (UPower), read directly off D-Bus with zbus rather than
//! shelling out.
//!
//! `sysinfo::battery()` used to read `/sys/class/power_supply` directly:
//! first device of type "Battery", no aggregation across multiple
//! batteries, and no way to distinguish "no battery present" from "haven't
//! polled yet" — a desktop with no battery just left `bat_text` stuck at its
//! placeholder forever instead of hiding the chip. UPower's `DisplayDevice`
//! is the daemon's own composite view of all power sources (correct
//! aggregation for multi-battery laptops) and exposes `IsPresent`/`Type`,
//! so "no battery" is representable rather than inferred from an empty
//! sysfs listing. Same fix as applied to quickshell-d77's `batProc` and
//! already true of utumno's `Quickshell.Services.UPower` usage.

use std::sync::OnceLock;
use tokio::runtime::Runtime;
use zvariant::{OwnedValue, Value};

const UPOWER: &str = "org.freedesktop.UPower";
const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE_IF: &str = "org.freedesktop.UPower.Device";

/// UPower's `Device.State` enum (org.freedesktop.UPower.Device).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Unknown,
    Charging,
    Discharging,
    Empty,
    FullyCharged,
    PendingCharge,
    PendingDischarge,
}

impl From<u32> for State {
    fn from(v: u32) -> Self {
        match v {
            1 => State::Charging,
            2 => State::Discharging,
            3 => State::Empty,
            4 => State::FullyCharged,
            5 => State::PendingCharge,
            6 => State::PendingDischarge,
            _ => State::Unknown,
        }
    }
}

pub struct BatteryInfo {
    pub percent: u8,
    pub state: State,
    pub time_to_empty_secs: u32,
    pub time_to_full_secs: u32,
}

fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime for upower status")
    })
}

fn get_property(conn: &zbus::Connection, path: &str, iface: &str, prop: &str) -> zbus::Result<OwnedValue> {
    let reply = rt().block_on(conn.call_method(
        Some(UPOWER),
        path,
        Some("org.freedesktop.DBus.Properties"),
        "Get",
        &(iface, prop),
    ))?;
    let val: OwnedValue = reply.body().deserialize()?;
    Ok(match &*val {
        Value::Value(inner) => OwnedValue::try_from(inner.as_ref()).unwrap_or(val),
        _ => val,
    })
}

/// `None` when there's no real battery to report: DisplayDevice's `Type`
/// is only `2` (Battery) when UPower actually has one aggregated in, and
/// `IsPresent` catches the (rare) case of a battery slot with nothing
/// physically in it. Mirrors the `Type == 2 && IsPresent` check
/// quickshell-d77's `batProc`/`g.hasBattery` now do, and what Quickshell's
/// own `UPowerDevice.isLaptopBattery` amounts to for utumno.
pub fn battery() -> Option<BatteryInfo> {
    let conn = rt().block_on(zbus::Connection::system()).ok()?;
    let prop = |name: &str| get_property(&conn, DISPLAY_DEVICE, DEVICE_IF, name);

    let kind = u32::try_from(&prop("Type").ok()?).unwrap_or(0);
    let present = bool::try_from(&prop("IsPresent").ok()?).unwrap_or(false);
    if kind != 2 || !present {
        return None;
    }

    let percent = f64::try_from(&prop("Percentage").ok()?).unwrap_or(0.0).round() as u8;
    let state = State::from(u32::try_from(&prop("State").ok()?).unwrap_or(0));
    let time_to_empty_secs = prop("TimeToEmpty")
        .ok()
        .and_then(|v| i64::try_from(&v).ok())
        .map(|v| v.max(0) as u32)
        .unwrap_or(0);
    let time_to_full_secs = prop("TimeToFull")
        .ok()
        .and_then(|v| i64::try_from(&v).ok())
        .map(|v| v.max(0) as u32)
        .unwrap_or(0);

    Some(BatteryInfo {
        percent,
        state,
        time_to_empty_secs,
        time_to_full_secs,
    })
}
