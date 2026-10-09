//! The default output and input, natively: read from the sound server at
//! start and again on its sink, source, server and card events.
//!
//! Emits `NAME` = {sink: {text, info, muted, volume}, source: {...}}.

use super::pulse::{Change, Device, Pulse};
use super::Hub;
use crate::config::Source;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const VOL: [char; 3] = ['\u{f026}', '\u{f027}', '\u{f028}'];
const HEADPHONES: char = '\u{f025}';
const MUTED: char = '\u{f6a9}';
const MIC: char = '\u{f130}';
const MIC_MUTED: char = '\u{f131}';

pub async fn run(src: Source, hub: Hub, pulse: Arc<Pulse>) {
    let mut events = pulse.events.subscribe();
    loop {
        match pulse.defaults().await {
            Ok((sink, source)) => hub.emit(&src.name, render(&sink, &source)),
            Err(e) => eprintln!("{}: {e:#}", src.name),
        }
        // wait for a change that matters; client events are not sent
        loop {
            match events.recv().await {
                Ok(Change::Sink | Change::Source | Change::Server | Change::Card | Change::Connected) => break,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                Err(_) => return,
            }
        }
        // a burst (volume steps, a device switch) is one read
        tokio::time::sleep(Duration::from_millis(hub.cfg.bar.coalesce_ms)).await;
        while let Ok(Ok(_)) = tokio::time::timeout(Duration::from_millis(1), events.recv()).await {}
    }
}

fn render(sink: &Device, source: &Device) -> serde_json::Value {
    let icon = if sink.muted {
        MUTED
    } else if sink.headphones {
        HEADPHONES
    } else if sink.volume > 66 {
        VOL[2]
    } else if sink.volume > 33 {
        VOL[1]
    } else {
        VOL[0]
    };
    let level = |d: &Device| {
        if d.muted {
            "Muted".to_string()
        } else {
            format!("Volume: {}%", d.volume)
        }
    };
    json!({
        "sink": {
            "text": if sink.muted { icon.to_string() } else { format!("{icon} {}%", sink.volume) },
            "info": format!("Device: {}\n{}", sink.description, level(sink)),
            "muted": sink.muted,
            "volume": sink.volume,
        },
        "source": {
            "text": (if source.muted { MIC_MUTED } else { MIC }).to_string(),
            "info": format!("Device: {}\n{}", source.description, level(source)),
            "muted": source.muted,
            "volume": source.volume,
        },
    })
}
