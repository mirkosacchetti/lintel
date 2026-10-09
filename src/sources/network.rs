//! The network: wifi through iwd's D-Bus objects (device, station, the
//! connected network's name, the station diagnostics for the signal),
//! else a wired interface that is up; the address from the kernel's
//! interface list, the gateway from the routing table. Taken on
//! rtnetlink messages, iwd's signals and when the card opens.
//!
//! {text: "ICON", info: "wlan0: ssid\nSignal: -44 dBm, 5.2 GHz\nIP:
//! 192.0.2.10/24\nGateway: 192.0.2.1"}

use super::dbus;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

const WIFI: char = '\u{f1eb}';
const ETHERNET: char = '\u{1f5a7}';
const NO_WIFI: char = '\u{f092d}';

struct Wifi {
    device: String,
    connected: bool,
    ssid: String,
    rssi: Option<i64>,
    frequency: Option<u64>,
}

async fn wifi() -> Option<Wifi> {
    let conn = dbus::system().await.ok()?;
    let objects = dbus::managed_objects(&conn, "net.connman.iwd").await.ok()?;
    // the first device in station mode, and its Station interface
    let (path, device) = objects.iter().find_map(|(path, ifaces)| {
        let d = ifaces.get("net.connman.iwd.Device")?;
        (dbus::string(d.get("Mode")) == "station").then(|| (path.clone(), dbus::string(d.get("Name"))))
    })?;
    let station = objects.get(&path)?.get("net.connman.iwd.Station")?;
    let connected = dbus::string(station.get("State")) == "connected";
    let ssid = station
        .get("ConnectedNetwork")
        .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v.try_clone().ok()?).ok())
        .and_then(|p| objects.get(&p)?.get("net.connman.iwd.Network").map(|n| dbus::string(n.get("Name"))))
        .unwrap_or_default();
    let mut rssi = None;
    let mut frequency = None;
    if connected {
        if let Ok(reply) = conn
            .call_method(
                Some("net.connman.iwd"),
                path.as_str(),
                Some("net.connman.iwd.StationDiagnostic"),
                "GetDiagnostics",
                &(),
            )
            .await
        {
            if let Ok(d) = reply.body().deserialize::<HashMap<String, OwnedValue>>() {
                rssi = d.get("RSSI").and_then(|v| i16::try_from(v.try_clone().ok()?).ok()).map(i64::from);
                frequency = d
                    .get("Frequency")
                    .and_then(|v| u32::try_from(v.try_clone().ok()?).ok())
                    .map(u64::from);
            }
        }
    }
    Some(Wifi {
        device,
        connected,
        ssid,
        rssi,
        frequency,
    })
}

/// "192.0.2.10/24" of an interface.
fn ip4(iface: &str) -> Option<String> {
    for ifa in nix::ifaddrs::getifaddrs().ok()? {
        if ifa.interface_name != iface {
            continue;
        }
        let (Some(addr), Some(mask)) = (ifa.address, ifa.netmask) else {
            continue;
        };
        let (Some(a), Some(m)) = (addr.as_sockaddr_in(), mask.as_sockaddr_in()) else {
            continue;
        };
        return Some(format!("{}/{}", a.ip(), u32::from(m.ip()).count_ones()));
    }
    None
}

/// The default route's gateway, from /proc/net/route (little-endian hex).
fn gateway() -> Option<String> {
    let text = std::fs::read_to_string("/proc/net/route").ok()?;
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() > 2 && f[1] == "00000000" {
            let g = u32::from_str_radix(f[2], 16).ok()?;
            let b = g.to_le_bytes();
            return Some(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]));
        }
    }
    None
}

/// A wired interface that is up.
fn ethernet() -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/class/net")
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("en") || n.starts_with("eth"))
        .filter(|n| {
            std::fs::read_to_string(format!("/sys/class/net/{n}/operstate"))
                .map(|s| s.trim() == "up")
                .unwrap_or(false)
        })
        .collect();
    names.sort();
    names.into_iter().next()
}

pub async fn take() -> Result<Value> {
    let w = wifi().await;
    let (icon, info) = match w {
        Some(w) if w.connected => {
            let mut info = format!("{}: {}", w.device, w.ssid);
            if let Some(r) = w.rssi {
                info += &format!("\nSignal: {r} dBm");
                if let Some(f) = w.frequency {
                    info += &format!(", {:.1} GHz", f as f64 / 1000.0);
                }
            }
            if let Some(a) = ip4(&w.device) {
                info += &format!("\nIP: {a}");
            }
            if let Some(g) = gateway() {
                info += &format!("\nGateway: {g}");
            }
            (WIFI, info)
        }
        _ => match ethernet() {
            Some(eth) => (
                ETHERNET,
                format!(
                    "{eth}\nIP: {}\nGateway: {}",
                    ip4(&eth).unwrap_or_default(),
                    gateway().unwrap_or_default()
                ),
            ),
            None => (NO_WIFI, "Disconnected".to_string()),
        },
    };
    Ok(json!({"text": icon.to_string(), "info": info}))
}
