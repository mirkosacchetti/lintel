//! The media player, natively, on playerctld: the active player is the
//! one used last, `playerctld shift` cycles them. playerctld's
//! PropertiesChanged (track, status, player list) and the sound server's
//! stream events (the app's own volume) are the triggers.
//!
//! Emits `NAME` = {text, info, class}: the track as pango markup (italic
//! when paused), and in the card the track, the album, the player and the
//! app's audio level as pavucontrol shows it.

use super::pulse::{Change, Pulse, Stream};
use super::Hub;
use crate::config::Source;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const PLAY: char = '\u{f04b}';
const PAUSE: char = '\u{f04c}';
const DEST: &str = "org.mpris.MediaPlayer2.playerctld";
const PATH: &str = "/org/mpris/MediaPlayer2";

pub async fn run(src: Source, hub: Hub, pulse: Arc<Pulse>) {
    loop {
        if let Err(e) = follow(&src, &hub, &pulse).await {
            eprintln!("{}: {e:#}; retrying in 5s", src.name);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn follow(src: &Source, hub: &Hub, pulse: &Arc<Pulse>) -> Result<()> {
    let conn = Connection::session().await?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(DEST)?
        .path(PATH)?
        .interface("org.freedesktop.DBus.Properties")?
        .member("PropertiesChanged")?
        .build();
    let mut signals = zbus::MessageStream::for_match_rule(rule, &conn, Some(32)).await?;
    let mut streams = pulse.events.subscribe();
    let coalesce = Duration::from_millis(hub.cfg.bar.coalesce_ms);
    loop {
        match read(&conn, pulse).await {
            Ok(v) => hub.emit(&src.name, v),
            Err(e) => eprintln!("{}: {e:#}", src.name),
        }
        tokio::select! {
            s = signals.next() => { s.context("signal stream ended")??; }
            e = streams.recv() => {
                match e {
                    Ok(Change::SinkInput | Change::Connected) => {}
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => anyhow::bail!("pulse gone"),
                }
            }
        }
        tokio::time::sleep(coalesce).await;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(1), signals.next()).await {}
        while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(1), streams.recv()).await {}
    }
}

async fn prop(conn: &Connection, iface: &str, name: &str) -> Option<OwnedValue> {
    let reply = conn
        .call_method(Some(DEST), PATH, Some("org.freedesktop.DBus.Properties"), "Get", &(iface, name))
        .await
        .ok()?;
    reply.body().deserialize::<Value>().ok().and_then(|v| v.try_to_owned().ok())
}

fn string(v: Option<OwnedValue>) -> String {
    v.and_then(|v| String::try_from(v).ok()).unwrap_or_default()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

async fn read(conn: &Connection, pulse: &Arc<Pulse>) -> Result<serde_json::Value> {
    let empty = json!({"text": "", "info": "", "class": ""});
    // playerctld's list, the active one first, without the ".instance_N"
    // suffix browsers add
    let names: Vec<String> = prop(conn, "com.github.altdesktop.playerctld", "PlayerNames")
        .await
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|n| {
            n.trim_start_matches("org.mpris.MediaPlayer2.")
                .split(".instance")
                .next()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    let Some(active) = names.first().cloned() else { return Ok(empty) };
    let status = string(prop(conn, "org.mpris.MediaPlayer2.Player", "PlaybackStatus").await);
    if status != "Playing" && status != "Paused" {
        return Ok(empty);
    }
    let meta: HashMap<String, OwnedValue> = prop(conn, "org.mpris.MediaPlayer2.Player", "Metadata")
        .await
        .and_then(|v| HashMap::try_from(v).ok())
        .unwrap_or_default();
    let get = |k: &str| {
        meta.get(k)
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_default()
    };
    let artist = meta
        .get("xesam:artist")
        .and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok())
        .map(|a| a.join(", "))
        .unwrap_or_default();
    let title = get("xesam:title");
    let album = get("xesam:album");

    // whole: the module's `max_chars` is the width in the bar, and the
    // card's label ellipsizes on its own
    let track = match (artist.is_empty(), title.is_empty()) {
        (false, false) => format!("{artist} - {title}"),
        (false, true) => artist,
        _ => title,
    };
    let text = if status == "Playing" {
        format!("{PLAY} {}", esc(&track))
    } else {
        format!("{PAUSE} <i>{}</i>", esc(&track))
    };

    // the app's own stream, else the player's MPRIS volume
    let volume = match stream_of(&active, pulse).await {
        Some(s) => Some(format!("{}%{}", s.volume, if s.muted { " (muted)" } else { "" })),
        None => prop(conn, "org.mpris.MediaPlayer2.Player", "Volume")
            .await
            .and_then(|v| f64::try_from(v).ok())
            .map(|v| format!("{}% (player)", (v * 100.0).round() as i64)),
    };
    let mut info = text.clone();
    if !album.is_empty() {
        info += &format!("\nAlbum: {}", esc(&album));
    }
    info += &format!("\nPlayer: {active}");
    if let Some(v) = volume {
        info += &format!("\nVolume: {v}");
    }
    Ok(json!({"text": text, "info": info, "class": status.to_lowercase()}))
}

/// The player's output stream: its MPRIS name is normally the binary
/// (spotify, firefox, cmus), else contained in the app name (Telegram's
/// MPRIS name is tdesktop, its stream "Telegram Desktop"); with several
/// streams (browser tabs) prefer a running one.
async fn stream_of(player: &str, pulse: &Arc<Pulse>) -> Option<Stream> {
    let name = if player == "tdesktop" {
        "telegram".to_string()
    } else {
        player.to_lowercase()
    };
    let mut found: Vec<Stream> = pulse
        .streams()
        .await
        .ok()?
        .into_iter()
        .filter(|s| s.binary == name || s.app_name.to_lowercase().contains(&name) || s.node_name.to_lowercase().contains(&name))
        .collect();
    found.sort_by_key(|s| s.corked);
    found.into_iter().next()
}
