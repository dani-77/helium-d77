//! Minimal NetworkManager status query.
//!
//! `helium_wsl::services::network::status()` deserializes `GetDevices`'s
//! reply (D-Bus signature `ao`) as `Vec<OwnedValue>`, but NetworkManager
//! returns a plain array of object paths, not variants — zbus rejects that
//! with "Signature mismatch: got 'ao', expected 'av'" and the call always
//! errors out. This reimplements just enough of the same NetworkManager
//! D-Bus calls with the correct `Vec<OwnedObjectPath>` type instead.

use std::sync::OnceLock;
use tokio::runtime::Runtime;
use zvariant::{OwnedObjectPath, OwnedValue, Value};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IF: &str = "org.freedesktop.NetworkManager";
const DEV_IF: &str = "org.freedesktop.NetworkManager.Device";
const WIFI_IF: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const AP_IF: &str = "org.freedesktop.NetworkManager.AccessPoint";

pub struct NetInfo {
    pub connected: bool,
    pub ssid: Option<String>,
    pub signal_strength: Option<u8>,
}

fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime for network status")
    })
}

fn get_property(conn: &zbus::Connection, path: &str, iface: &str, prop: &str) -> zbus::Result<OwnedValue> {
    let reply = rt().block_on(conn.call_method(
        Some(NM),
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

pub fn status() -> zbus::Result<NetInfo> {
    let conn = rt().block_on(zbus::Connection::system())?;

    let state_val = get_property(&conn, NM_PATH, NM_IF, "State")?;
    let state = u32::try_from(&state_val).unwrap_or(0);
    let connected = matches!(state, 50 | 60 | 70);

    let reply = rt().block_on(conn.call_method(Some(NM), NM_PATH, Some(NM_IF), "GetDevices", &()))?;
    let devices: Vec<OwnedObjectPath> = reply.body().deserialize()?;

    let mut ssid = None;
    let mut signal_strength = None;
    let mut has_wired = false;
    for dev in &devices {
        let Ok(dtype_val) = get_property(&conn, dev.as_str(), DEV_IF, "DeviceType") else { continue };
        match u32::try_from(&dtype_val).unwrap_or(0) {
            2 => {
                if let Ok(ap_val) = get_property(&conn, dev.as_str(), WIFI_IF, "ActiveAccessPoint") {
                    if let Value::ObjectPath(ap_path) = &*ap_val {
                        if !ap_path.as_str().is_empty() && ap_path.as_str() != "/" {
                            if let Ok(ssid_val) = get_property(&conn, ap_path.as_str(), AP_IF, "Ssid") {
                                if let Value::Array(arr) = &*ssid_val {
                                    let bytes: Vec<u8> = arr
                                        .inner()
                                        .iter()
                                        .filter_map(|v| if let Value::U8(b) = v { Some(*b) } else { None })
                                        .collect();
                                    ssid = Some(String::from_utf8_lossy(&bytes).trim_matches('\0').to_string());
                                }
                            }
                            if let Ok(strength_val) = get_property(&conn, ap_path.as_str(), AP_IF, "Strength") {
                                if let Value::U8(s) = &*strength_val {
                                    signal_strength = Some(*s);
                                }
                            }
                        }
                    }
                }
            }
            1 => {
                if let Ok(dev_state_val) = get_property(&conn, dev.as_str(), DEV_IF, "State") {
                    if u32::try_from(&dev_state_val).unwrap_or(0) >= 100 {
                        has_wired = true;
                    }
                }
            }
            _ => {}
        }
    }

    Ok(NetInfo {
        connected,
        ssid: ssid.or(if has_wired { Some("wired".to_string()) } else { None }),
        signal_strength,
    })
}
