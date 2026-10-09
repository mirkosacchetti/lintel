//! The sources: everything that produces a variable's value. They run on
//! the tokio side; values cross to the GTK side as `Update`s.

pub mod audio;
pub mod battery;
pub mod bluetooth;
pub mod brightness;
pub mod clock;
pub mod command;
pub mod dbus;
pub mod display;
pub mod mpris;
pub mod network;
pub mod nightlight;
pub mod notify;
pub mod notifyd;
pub mod picker;
pub mod pomodoro;
pub mod powerprofile;
pub mod pulse;
pub mod snapshot;
pub mod sway;
pub mod tray;
pub mod triggers;

use crate::config::{Config, Source};
use crate::ipc::Request;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc};

/// A variable changed.
#[derive(Debug, Clone)]
pub struct Update {
    pub name: String,
    pub value: Value,
}

/// What crosses to the GTK side.
#[derive(Debug)]
pub enum Message {
    Var(Update),
    Tray(Box<tray::TrayUpdate>),
    /// A list to pick from; the choice comes back on the request's channel.
    Pick(Box<picker::PickRequest>),
    /// Close the picker, if open.
    CancelPick,
    Notify(notifyd::NotifyUpdate),
}

/// A request to take a snapshot again: from `lintel refresh` or a hover.
#[derive(Debug, Clone)]
pub struct Refresh {
    pub name: String,
    pub hover: bool,
}

/// What every source gets: a way to emit, the config, the refresh bus.
#[derive(Clone)]
pub struct Hub {
    out: async_channel::Sender<Message>,
    /// The latest values, for `lintel get` (and the sources themselves).
    pub values: Arc<Mutex<HashMap<String, Value>>>,
    pub refresh: broadcast::Sender<Refresh>,
    pub cfg: Arc<Config>,
}

impl Hub {
    pub fn emit(&self, name: &str, value: Value) {
        self.values.lock().unwrap().insert(name.to_string(), value.clone());
        let _ = self.out.send_blocking(Message::Var(Update {
            name: name.to_string(),
            value,
        }));
    }

    pub fn tray(&self, update: tray::TrayUpdate) {
        let _ = self.out.send_blocking(Message::Tray(Box::new(update)));
    }

    pub fn notify(&self, update: notifyd::NotifyUpdate) {
        let _ = self.out.send_blocking(Message::Notify(update));
    }

    /// Show a list, wait for the choice.
    pub async fn pick(&self, name: &str, prompt: &str, query: &str, items: Vec<picker::Item>) -> picker::Choice {
        let (reply, rx) = tokio::sync::oneshot::channel();
        let req = picker::PickRequest {
            name: name.to_string(),
            prompt: prompt.to_string(),
            query: query.to_string(),
            items,
            reply,
        };
        if self.out.send(Message::Pick(Box::new(req))).await.is_err() {
            return picker::Choice::Cancelled;
        }
        rx.await.unwrap_or(picker::Choice::Cancelled)
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        self.values.lock().unwrap().get(name).cloned()
    }

    pub fn scripts(&self) -> &str {
        &self.cfg.bar.scripts
    }
}

/// The launcher (every app, its open windows under it) and the window
/// switcher (the windows alone), one Ctrl+Tab away from each other.
async fn launcher(hub: Hub, mut windows: bool, query: String) -> String {
    let apps = tokio::task::spawn_blocking(picker::desktop_entries).await.unwrap_or_default();
    loop {
        let list = picker::windows().await.unwrap_or_default();
        let choice = if windows {
            match hub.pick("windows", "Windows", &query, picker::window_items(&list, &apps)).await {
                picker::Choice::Picked(i) => {
                    if let Err(e) = picker::focus(&list[i], &hub).await {
                        return format!("error: {e}");
                    }
                    return "ok".into();
                }
                c => c,
            }
        } else {
            let (items, targets) = picker::launch_items(&apps, &list);
            match hub.pick("launch", "Run", &query, items).await {
                picker::Choice::Picked(i) => {
                    match targets[i] {
                        picker::Target::App(a) => picker::launch(&apps[a], &hub),
                        picker::Target::Window(w) => {
                            if let Err(e) = picker::focus(&list[w], &hub).await {
                                return format!("error: {e}");
                            }
                        }
                    }
                    return "ok".into();
                }
                c => c,
            }
        };
        match choice {
            picker::Choice::Switch => windows = !windows,
            _ => return String::new(),
        }
    }
}

/// Start every source and serve the requests until `Quit`.
pub async fn run(cfg: Arc<Config>, out: async_channel::Sender<Message>, mut reqs: mpsc::UnboundedReceiver<Request>) {
    let (refresh, _) = broadcast::channel(64);
    let hub = Hub {
        out,
        values: Arc::default(),
        refresh,
        cfg: cfg.clone(),
    };

    let mut pomodoro_tx = None;
    let mut timer_tx = None;
    // the notification daemon, before the sources that notify
    let (notify_events, notify_commands) = if cfg.notifications.enabled {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        tokio::spawn(notifyd::run(hub.clone(), ev_rx, cmd_rx));
        (Some(ev_tx), Some(cmd_tx))
    } else {
        (None, None)
    };
    // one sound-server connection, when a source needs it
    let pulse = if cfg.sources.iter().any(|s| matches!(s.kind.as_str(), "audio" | "mpris")) {
        Some(pulse::Pulse::start())
    } else {
        None
    };
    // the tray only when a module shows it
    let tray_tx = if cfg.modules.iter().any(|m| m.kind == "tray") {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(tray::run(hub.clone(), rx));
        Some(tx)
    } else {
        None
    };
    for src in &cfg.sources {
        hub.emit(&src.name, Config::initial_value(src));
        let src: Source = src.clone();
        let hub = hub.clone();
        match src.kind.as_str() {
            "listen" => {
                tokio::spawn(command::listen(src, hub));
            }
            "command" => {
                tokio::spawn(command::snapshot(src, hub));
            }
            "clock" => {
                tokio::spawn(clock::run(src, hub));
            }
            "sway" => {
                tokio::spawn(sway::run(src, hub));
            }
            "display" => {
                tokio::spawn(display::run(src, hub));
            }
            "audio" => {
                tokio::spawn(audio::run(src, hub, pulse.clone().expect("pulse")));
            }
            "mpris" => {
                tokio::spawn(mpris::run(src, hub, pulse.clone().expect("pulse")));
            }
            // the native snapshots: a Rust reader on the same triggers
            "battery" => {
                tokio::spawn(snapshot::run(src, hub, |_, _| async { battery::take() }));
            }
            "network" => {
                tokio::spawn(snapshot::run(src, hub, |_, _| network::take()));
            }
            "bluetooth" => {
                tokio::spawn(snapshot::run(src, hub, |_, _| bluetooth::take()));
            }
            "brightness" => {
                tokio::spawn(snapshot::run(src, hub, brightness::take));
            }
            "powerprofile" => {
                tokio::spawn(snapshot::run(src, hub, |_, _| powerprofile::take()));
            }
            "nightlight" => {
                tokio::spawn(snapshot::run(src, hub, |_, _| nightlight::take()));
            }
            "pomodoro" => {
                let (tx, rx) = mpsc::unbounded_channel();
                pomodoro_tx = Some(tx.clone());
                tokio::spawn(pomodoro::run_pomodoro(src, hub, rx, tx));
            }
            "timer" => {
                let (tx, rx) = mpsc::unbounded_channel();
                timer_tx = Some(tx.clone());
                tokio::spawn(pomodoro::run_countdown(src, hub, rx, tx));
            }
            "static" => {}
            _ => {}
        }
    }

    // a signal is a quit too: the listeners' process groups go with the
    // runtime, instead of lingering until their next write fails
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
    loop {
        let req = tokio::select! {
            r = reqs.recv() => match r { Some(r) => r, None => break },
            _ = async { match term.as_mut() { Some(s) => s.recv().await, None => std::future::pending().await } } => Request::Quit,
            _ = async { match int.as_mut() { Some(s) => s.recv().await, None => std::future::pending().await } } => Request::Quit,
            _ = async { match hup.as_mut() { Some(s) => s.recv().await, None => std::future::pending().await } } => Request::Quit,
        };
        match req {
            Request::Refresh(name) => {
                let _ = hub.refresh.send(Refresh { name, hover: false });
            }
            Request::Hover(name) => {
                let _ = hub.refresh.send(Refresh { name, hover: true });
            }
            Request::Update(name, value) => hub.emit(&name, value),
            Request::Get(name, reply) => {
                let v = hub.get(&name).map(|v| v.to_string()).unwrap_or_else(|| "null".into());
                let _ = reply.send(v);
            }
            Request::State(reply) => {
                let all: serde_json::Map<String, Value> = hub.values.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                let _ = reply.send(Value::Object(all).to_string());
            }
            Request::Pomodoro(cmd, reply) => match &pomodoro_tx {
                Some(tx) => {
                    let _ = tx.send((cmd, reply));
                }
                None => {
                    let _ = reply.send("error: no pomodoro source in the config".into());
                }
            },
            Request::Timer(args, reply) => match &timer_tx {
                Some(tx) => {
                    let _ = tx.send((args.join(" "), reply));
                }
                None => {
                    let _ = reply.send("error: no timer source in the config".into());
                }
            },
            Request::Tray(cmd) => {
                if let Some(tx) = &tray_tx {
                    let _ = tx.send(cmd);
                }
            }
            Request::Pick {
                name,
                prompt,
                lines,
                reply,
            } => {
                let hub = hub.clone();
                tokio::spawn(async move {
                    let items = picker::dmenu_items(&lines);
                    let answer = match hub.pick(&name, &prompt, "", items).await {
                        picker::Choice::Picked(i) => format!("{i}\t{}", lines[i].split('\0').next().unwrap_or("")),
                        _ => String::new(),
                    };
                    let _ = reply.send(answer);
                });
            }
            Request::Cancel => {
                let _ = hub.out.send(Message::CancelPick).await;
            }
            Request::Notify(ev) => {
                if let Some(tx) = &notify_events {
                    let _ = tx.send(ev);
                }
            }
            Request::Notifications(cmd, reply) => match &notify_commands {
                Some(tx) => {
                    let _ = tx.send((cmd, reply));
                }
                None => {
                    let _ = reply.send("error: the notification daemon is off".into());
                }
            },
            Request::Launch(query, reply) => {
                let hub = hub.clone();
                tokio::spawn(async move {
                    let _ = reply.send(launcher(hub, false, query).await);
                });
            }
            Request::Windows(query, reply) => {
                let hub = hub.clone();
                tokio::spawn(async move {
                    let _ = reply.send(launcher(hub, true, query).await);
                });
            }
            Request::Quit => break,
        }
    }
}
