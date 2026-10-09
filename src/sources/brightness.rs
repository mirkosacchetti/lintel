//! The brightness of the screen in use: the picked one (the name in
//! `$XDG_RUNTIME_DIR/screen-pick`), else the focused output. A laptop
//! panel is read from its backlight in sysfs. An external monitor has no
//! backlight to read: it speaks DDC/CI over I2C, slow and particular to
//! each setup, so the source's `command` is asked for it instead, and its
//! output (JSON, or text) is the value. Taken on backlight uevents,
//! sway's output events and ticks, `lintel refresh` and the card
//! opening.
//!
//! {text: "ICON", info: "eDP-1\nLevel: 45%"}

use super::Hub;
use crate::config::Source;
use crate::runner;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;

const SUN: char = '\u{f185}';
const HALF: char = '\u{f00df}';
const HIGH: char = '\u{f00e0}';
const FULL: char = '\u{f111}';

fn icon(pct: i64) -> char {
    match pct {
        100.. => FULL,
        76..=99 => HIGH,
        51..=75 => HALF,
        _ => SUN,
    }
}

fn pick() -> String {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    std::fs::read_to_string(dir.join("screen-pick"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The panel's level from its backlight, in percent.
fn backlight() -> Option<i64> {
    let dir = std::fs::read_dir("/sys/class/backlight").ok()?.flatten().next()?.path();
    let read = |n: &str| std::fs::read_to_string(dir.join(n)).ok()?.trim().parse::<i64>().ok();
    let (v, max) = (read("brightness")?, read("max_brightness")?);
    (max > 0).then(|| (v * 100 + max / 2) / max)
}

pub async fn take(src: Source, hub: Hub) -> Result<Value> {
    // the screen: the picked one if still there, else the focused one
    let mut conn = swayipc_async::Connection::new().await?;
    let outputs = conn.get_outputs().await?;
    let mut active: Vec<_> = outputs.iter().filter(|o| o.active).collect();
    active.sort_by_key(|o| !o.focused);
    let pick = pick();
    let Some(screen) = active.iter().find(|o| o.name == pick).or(active.first()) else {
        return Ok(json!({"text": "", "info": ""}));
    };
    if !screen.name.starts_with("eDP") {
        // DDC/CI: the command's business, see above
        if src.command.is_empty() {
            return Ok(json!({"text": SUN.to_string(), "info": format!("{}\nNo brightness control", screen.name)}));
        }
        let out = runner::capture(&src.command, hub.scripts()).await?;
        return Ok(runner::parse_output(&out));
    }
    Ok(match backlight() {
        Some(pct) => json!({"text": icon(pct).to_string(), "info": format!("{}\nLevel: {pct}%", screen.name), "level": pct}),
        None => json!({"text": SUN.to_string(), "info": format!("{}\nNo backlight", screen.name)}),
    })
}
