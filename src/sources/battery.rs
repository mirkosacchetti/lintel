//! The battery, from sysfs: {text: "85% ICON", info: "85%, 1h 30min
//! left\nPower: 7.3 W at 15.65 V\nCycles: 30, health 101%\nCharge limits: 75-80%",
//! class: "" | warning | critical}. Taken on the kernel's power_supply
//! uevents, UPower's signals and when the card opens.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;

const CHARGING: char = '\u{f5e7}';
const PLUGGED: char = '\u{f1e6}';
const LEVELS: [char; 5] = ['\u{f244}', '\u{f243}', '\u{f242}', '\u{f241}', '\u{f240}'];

fn read_num(dir: &Path, name: &str) -> Option<i64> {
    std::fs::read_to_string(dir.join(name)).ok()?.trim().parse().ok()
}

/// "1h 05min" to move `wh` at `w` (both in the same unit).
fn hm(wh: i64, w: i64) -> Option<String> {
    if w <= 0 {
        return None;
    }
    let h = wh as f64 / w as f64;
    Some(format!("{}h {:02}min", h as i64, ((h - h.floor()) * 60.0) as i64))
}

fn read_str(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default().trim().to_string()
}

/// The machine's battery: a `Battery` of system scope that reports a
/// capacity. A mouse or a headset (hid, bluetooth) is a `Battery` too,
/// with `scope` = Device and no capacity, and readdir is in no order, so
/// the first entry would be the mouse half the time. Among several
/// (BAT0, BAT1) the first by name.
fn system_battery() -> Option<std::path::PathBuf> {
    let mut dirs: Vec<_> = std::fs::read_dir("/sys/class/power_supply")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| read_str(p, "type") == "Battery" && read_str(p, "scope") != "Device" && p.join("capacity").is_file())
        .collect();
    dirs.sort();
    dirs.into_iter().next()
}

pub fn take() -> Result<Value> {
    let Some(dir) = system_battery() else {
        return Ok(json!({"text": "", "info": "", "class": ""}));
    };
    let cap = read_num(&dir, "capacity").unwrap_or(0);
    let status = read_str(&dir, "status");
    // energy_* in µWh, or charge_* in µAh: the ratio is what matters
    let (now, full, rate) = match (
        read_num(&dir, "energy_now"),
        read_num(&dir, "energy_full"),
        read_num(&dir, "power_now"),
    ) {
        (Some(n), Some(f), Some(p)) => (n, f, p),
        _ => (
            read_num(&dir, "charge_now").unwrap_or(0),
            read_num(&dir, "charge_full").unwrap_or(0),
            read_num(&dir, "current_now").unwrap_or(0),
        ),
    };
    let (text, mut info) = match status.as_str() {
        "Charging" => (
            format!("{CHARGING} {cap}%"),
            format!("{cap}%, {} to full", hm(full - now, rate).unwrap_or_else(|| "?".into())),
        ),
        "Full" | "Not charging" => (format!("{PLUGGED} {cap}%"), format!("{cap}%, full")),
        _ => (
            format!("{cap}% {}", LEVELS[(cap.clamp(0, 100) * 4 / 100) as usize]),
            format!("{cap}%, {} left", hm(now, rate).unwrap_or_else(|| "?".into())),
        ),
    };
    // the draw now (µW, or µA times the voltage for charge_* drivers)
    let watts = match read_num(&dir, "power_now") {
        Some(p) => Some(p as f64 / 1e6),
        None => read_num(&dir, "current_now")
            .zip(read_num(&dir, "voltage_now"))
            .map(|(a, v)| a as f64 * v as f64 / 1e12),
    };
    let volts = read_num(&dir, "voltage_now").map(|v| v as f64 / 1e6);
    match (watts.filter(|w| *w > 0.0), volts) {
        (Some(w), Some(v)) => info += &format!("\nPower: {w:.1} W at {v:.2} V"),
        (Some(w), None) => info += &format!("\nPower: {w:.1} W"),
        (None, Some(v)) => info += &format!("\nVoltage: {v:.2} V"),
        (None, None) => {}
    }
    // wear: what a full charge holds against the design, and the cycles
    let cycles = read_num(&dir, "cycle_count").filter(|c| *c > 0);
    let design = read_num(&dir, "energy_full_design")
        .or_else(|| read_num(&dir, "charge_full_design"))
        .filter(|d| *d > 0);
    let health = design.map(|d| (full * 100 + d / 2) / d);
    match (cycles, health) {
        (Some(c), Some(h)) => info += &format!("\nCycles: {c}, health {h}%"),
        (Some(c), None) => info += &format!("\nCycles: {c}"),
        (None, Some(h)) => info += &format!("\nHealth: {h}%"),
        (None, None) => {}
    }
    // the charge limits, when set (0 and 100 mean none)
    let start = read_num(&dir, "charge_control_start_threshold").unwrap_or(0);
    let end = read_num(&dir, "charge_control_end_threshold").unwrap_or(100);
    if start > 0 || end < 100 {
        info += &format!("\nCharge limits: {start}-{end}%");
    }
    let class = if status != "Charging" && cap <= 15 {
        "critical"
    } else if status != "Charging" && cap <= 30 {
        "warning"
    } else {
        ""
    };
    Ok(
        json!({"text": text, "info": info, "class": class, "capacity": cap, "status": status, "watts": watts, "volts": volts, "cycles": cycles, "health": health}),
    )
}
