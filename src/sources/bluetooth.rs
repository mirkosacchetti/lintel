//! Bluetooth from bluez's objects: the adapter's power, the connected
//! devices with their battery, the picked one first (the address in
//! `$XDG_RUNTIME_DIR/bt-pick`, for a script that cycles them).
//! Taken on bluez's signals and `lintel refresh`.
//!
//! {text: "ICON", info: "Keyboard\nBattery: 85%" | "No device
//! connected" | "Off"}

use super::dbus;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;

const ON: char = '\u{f294}';
const OFF: char = '\u{f00b2}';
const CONNECTED: char = '\u{f00b0}';

fn pick() -> String {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    std::fs::read_to_string(dir.join("bt-pick")).unwrap_or_default().trim().to_string()
}

pub async fn take() -> Result<Value> {
    let conn = dbus::system().await?;
    let objects = match dbus::managed_objects(&conn, "org.bluez").await {
        Ok(o) => o,
        Err(_) => return Ok(json!({"text": OFF.to_string(), "info": "Off"})),
    };
    let powered = objects.values().any(|i| {
        i.get("org.bluez.Adapter1")
            .map(|a| dbus::boolean(a.get("Powered")))
            .unwrap_or(false)
    });
    if !powered {
        return Ok(json!({"text": OFF.to_string(), "info": "Off"}));
    }
    // connected devices: (address, name, battery)
    let mut devices: Vec<(String, String, Option<u8>)> = objects
        .values()
        .filter_map(|i| {
            let d = i.get("org.bluez.Device1")?;
            if !dbus::boolean(d.get("Connected")) {
                return None;
            }
            let name = {
                let alias = dbus::string(d.get("Alias"));
                if alias.is_empty() {
                    dbus::string(d.get("Name"))
                } else {
                    alias
                }
            };
            let battery = i
                .get("org.bluez.Battery1")
                .and_then(|b| u8::try_from(b.get("Percentage")?.try_clone().ok()?).ok());
            Some((dbus::string(d.get("Address")), name, battery))
        })
        .collect();
    devices.sort_by(|a, b| a.1.cmp(&b.1));
    let pick = pick();
    if let Some(i) = devices.iter().position(|(addr, _, _)| *addr == pick) {
        devices.rotate_left(i);
    }
    let Some((_, name, battery)) = devices.first() else {
        return Ok(json!({"text": ON.to_string(), "info": "No device connected", "connected": 0}));
    };
    let mut info = name.clone();
    if let Some(b) = battery {
        info += &format!("\nBattery: {b}%");
    }
    Ok(json!({"text": CONNECTED.to_string(), "info": info, "connected": devices.len()}))
}
