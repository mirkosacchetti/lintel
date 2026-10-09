//! The outputs, natively over sway's IPC: read at start and on sway's
//! output events and ticks (a script that picks a screen sends one).
//!
//! Emits `NAME` = {text, info, picked}: the card shows the picked output
//! (the name in `$XDG_RUNTIME_DIR/screen-pick`, shared with the
//! brightness source), else the focused one:
//! "DP-1 Dell U2723QE", then "3840x2160 @ 60 Hz, scale 1.5".

use super::Hub;
use crate::config::Source;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use swayipc_async::{EventType, Output};

const ICON: char = '\u{f0379}';

pub async fn run(src: Source, hub: Hub) {
    loop {
        if let Err(e) = follow(&src, &hub).await {
            eprintln!("{}: {e:#}; reconnecting in 2s", src.name);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn pick_file() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("screen-pick")
}

async fn follow(src: &Source, hub: &Hub) -> Result<()> {
    let mut conn = crate::swaysock::connect().await.context("connecting to sway")?;
    let mut events = crate::swaysock::connect()
        .await?
        .subscribe([EventType::Output, EventType::Tick])
        .await?;
    loop {
        let outputs = conn.get_outputs().await?;
        let pick = std::fs::read_to_string(pick_file()).unwrap_or_default().trim().to_string();
        hub.emit(&src.name, render(&outputs, &pick));
        match events.next().await {
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
            None => anyhow::bail!("event stream ended"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(1), events.next()).await {}
    }
}

fn render(outputs: &[Output], pick: &str) -> serde_json::Value {
    // the active outputs, the focused one first, then rotated so the
    // picked one (if still there) leads
    let mut active: Vec<&Output> = outputs.iter().filter(|o| o.active).collect();
    active.sort_by_key(|o| !o.focused);
    if let Some(i) = active.iter().position(|o| o.name == pick) {
        active.rotate_left(i);
    }
    let lines: Vec<String> = active
        .iter()
        .map(|o| {
            let label = if o.name.starts_with("eDP") {
                o.name.clone()
            } else {
                format!("{} {}", o.name, o.model)
            };
            let Some(m) = &o.current_mode else { return label };
            format!(
                "{label}\n{}x{} @ {} Hz, scale {}",
                m.width,
                m.height,
                (m.refresh as f64 / 1000.0).round() as i64,
                o.scale.unwrap_or(1.0),
            )
        })
        .collect();
    json!({
        "text": ICON.to_string(),
        "info": lines.first().cloned().unwrap_or_default(),
        "picked": active.first().map(|o| o.name.clone()).unwrap_or_default(),
    })
}
