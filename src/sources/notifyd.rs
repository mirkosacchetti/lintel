//! The notification daemon: the bar owns org.freedesktop.Notifications on
//! the session bus and draws the notifications itself (ui/notify.rs).
//! Urgencies with their
//! timeouts, replacing by id or by stack tag, icons from the hints or the
//! app's desktop entry, actions as buttons, a history, a pause. And what
//! only a bar can do: a click on a notification goes to the app's window
//! (its workspace, its scratchpad), after invoking the app's default
//! action, so a mail alert lands on the mail client and a Telegram
//! message on the conversation; the bar's own notifications (pomodoro,
//! countdowns) carry buttons that call the bar directly, no bus between.
//!
//! Timers are deadlines: the task sleeps until the next expiry or the
//! next message. Emits `notifications` = {text, info, class, count,
//! paused} for a module that wants a bell.

use super::picker;
use super::Hub;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedValue, Value};
use zbus::Connection;

const PATH: &str = "/org/freedesktop/Notifications";

/// A raw image from the image-data hint: RGB(A) rows.
#[derive(Debug, Clone)]
pub struct Image {
    pub width: i32,
    pub height: i32,
    pub rowstride: i32,
    pub has_alpha: bool,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Note {
    pub id: u32,
    pub app: String,
    pub summary: String,
    pub body: String,
    /// An icon name, or a file path; may be empty.
    pub icon: String,
    pub image: Option<Image>,
    /// (key, label); "default" is the click, the others are buttons.
    pub actions: Vec<(String, String)>,
    /// 0 low, 1 normal, 2 critical.
    pub urgency: u8,
    /// None: stays until closed.
    pub timeout: Option<Duration>,
    /// The progress hint, 0..=100.
    pub value: Option<i32>,
    pub desktop_entry: String,
    pub transient: bool,
    pub tag: String,
    pub time: u64,
}

/// To the GTK side.
#[derive(Debug)]
pub enum NotifyUpdate {
    /// New, or replaced in place when the id is already shown.
    Show(Box<Note>),
    Close(u32),
}

/// From the GTK side.
#[derive(Debug)]
pub enum NotifyEvent {
    /// Left click: the default action, the app's window, then close.
    Clicked(u32),
    /// A button.
    Action(u32, String),
    /// Right click.
    Dismiss(u32),
    /// Middle click.
    DismissAll,
}

/// A notification of the bar's own, posted in-process.
pub struct Internal {
    pub summary: String,
    pub body: String,
    pub urgency: u8,
    /// Milliseconds; -1 the urgency's default, 0 never.
    pub timeout_ms: i32,
    pub icon: String,
    pub actions: Vec<(String, String)>,
    pub on_action: Option<ActionHandler>,
}

/// What a button of the bar's own notification calls, with its key.
pub type ActionHandler = Arc<dyn Fn(&str) + Send + Sync>;

static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static INTERNAL: OnceLock<mpsc::UnboundedSender<Internal>> = OnceLock::new();

/// Post a notification of the bar's own; false when the daemon is off
/// (the caller then goes over the bus).
pub fn post(n: Internal) -> bool {
    match INTERNAL.get() {
        Some(tx) => tx.send(n).is_ok(),
        None => false,
    }
}

// ---- the bus interface --------------------------------------------------

enum Incoming {
    Note(Box<Note>),
    CloseRequest(u32),
}

struct Server {
    tx: mpsc::UnboundedSender<Incoming>,
}

fn hint_str(h: &HashMap<String, OwnedValue>, key: &str) -> String {
    h.get(key)
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

fn hint_bool(h: &HashMap<String, OwnedValue>, key: &str) -> bool {
    match h.get(key).map(|v| &**v) {
        Some(Value::Bool(b)) => *b,
        Some(_) => hint_i32(h, key).unwrap_or(0) != 0,
        None => false,
    }
}

/// A number hint, whatever integer type the app chose (urgency is a
/// byte, value an int32, some send int64).
fn hint_i32(h: &HashMap<String, OwnedValue>, key: &str) -> Option<i32> {
    match &**h.get(key)? {
        Value::U8(v) => Some(i32::from(*v)),
        Value::I16(v) => Some(i32::from(*v)),
        Value::U16(v) => Some(i32::from(*v)),
        Value::I32(v) => Some(*v),
        Value::U32(v) => Some(*v as i32),
        Value::I64(v) => Some(*v as i32),
        Value::U64(v) => Some(*v as i32),
        _ => None,
    }
}

/// image-data: (iiibiiay) width, height, rowstride, has alpha, bits per
/// sample, channels, data.
fn hint_image(h: &HashMap<String, OwnedValue>) -> Option<Image> {
    let v = h.get("image-data").or_else(|| h.get("image_data")).or_else(|| h.get("icon_data"))?;
    let Value::Structure(s) = &**v else { return None };
    let f = s.fields();
    if f.len() < 7 {
        return None;
    }
    let width = i32::try_from(f[0].clone()).ok()?;
    let height = i32::try_from(f[1].clone()).ok()?;
    let rowstride = i32::try_from(f[2].clone()).ok()?;
    let has_alpha = bool::try_from(f[3].clone()).ok()?;
    let bits = i32::try_from(f[4].clone()).ok()?;
    let data = Vec::<u8>::try_from(f[6].clone()).ok()?;
    if bits != 8 || width <= 0 || height <= 0 || data.len() < (rowstride * (height - 1) + width * if has_alpha { 4 } else { 3 }) as usize {
        return None;
    }
    Some(Image {
        width,
        height,
        rowstride,
        has_alpha,
        data,
    })
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    fn get_capabilities(&self) -> Vec<String> {
        ["body", "body-markup", "actions", "icon-static", "persistence", "x-dunst-stack-tag"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        };
        let urgency = hint_i32(&hints, "urgency").unwrap_or(1).clamp(0, 2) as u8;
        let image_path = hint_str(&hints, "image-path");
        let image_path = if image_path.is_empty() {
            hint_str(&hints, "image_path")
        } else {
            image_path
        };
        let icon = if !image_path.is_empty() { image_path } else { app_icon };
        let tag = {
            let t = hint_str(&hints, "x-dunst-stack-tag");
            if t.is_empty() {
                hint_str(&hints, "x-canonical-private-synchronous")
            } else {
                t
            }
        };
        let note = Note {
            id,
            app: app_name,
            summary,
            body,
            icon: icon.strip_prefix("file://").map(String::from).unwrap_or(icon),
            image: hint_image(&hints),
            actions: actions
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| (c[0].clone(), c[1].clone()))
                .collect(),
            urgency,
            timeout: match expire_timeout {
                0 => None,
                t if t > 0 => Some(Duration::from_millis(t as u64)),
                _ => Some(Duration::ZERO), // the urgency's default, filled by the daemon
            },
            value: hint_i32(&hints, "value"),
            desktop_entry: hint_str(&hints, "desktop-entry"),
            transient: hint_bool(&hints, "transient"),
            tag,
            time: now(),
        };
        let _ = self.tx.send(Incoming::Note(Box::new(note)));
        id
    }

    fn close_notification(&self, id: u32) {
        let _ = self.tx.send(Incoming::CloseRequest(id));
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        ("lintel".into(), "lintel".into(), env!("CARGO_PKG_VERSION").into(), "1.2".into())
    }

    #[zbus(signal)]
    async fn notification_closed(emitter: &SignalEmitter<'_>, id: u32, reason: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(emitter: &SignalEmitter<'_>, id: u32, action_key: String) -> zbus::Result<()>;
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

// ---- the daemon ---------------------------------------------------------

/// Why a notification closed, as the spec numbers them.
const EXPIRED: u32 = 1;
const DISMISSED: u32 = 2;
const CLOSED_BY_CALL: u32 = 3;

pub type Command = (String, oneshot::Sender<String>);

struct Daemon {
    hub: Hub,
    conn: Connection,
    visible: Vec<Note>,
    queue: VecDeque<Note>,
    history: VecDeque<Note>,
    paused: bool,
    deadlines: HashMap<u32, Instant>,
    /// The bar's own notifications: what their buttons call.
    handlers: HashMap<u32, ActionHandler>,
    /// Desktop entry id, lowercase, to its icon: for apps that send none.
    entry_icons: HashMap<String, String>,
}

pub async fn run(hub: Hub, mut events: mpsc::UnboundedReceiver<NotifyEvent>, mut commands: mpsc::UnboundedReceiver<Command>) {
    let (internal_tx, mut internal) = mpsc::unbounded_channel::<Internal>();
    let _ = INTERNAL.set(internal_tx);
    let (tx, mut incoming) = mpsc::unbounded_channel::<Incoming>();
    let conn = loop {
        match serve(tx.clone()).await {
            Ok(c) => break c,
            Err(e) => {
                eprintln!("notifications: {e:#}; retrying in 5s (is another daemon running?)");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    };
    let entry_icons = tokio::task::spawn_blocking(|| {
        picker::desktop_entries()
            .into_iter()
            .map(|a| (a.id.to_lowercase(), a.icon))
            .collect::<HashMap<_, _>>()
    })
    .await
    .unwrap_or_default();
    let mut d = Daemon {
        hub,
        conn,
        visible: Vec::new(),
        queue: VecDeque::new(),
        history: VecDeque::new(),
        paused: false,
        deadlines: HashMap::new(),
        handlers: HashMap::new(),
        entry_icons,
    };
    d.publish();
    loop {
        let next = d.deadlines.values().min().copied();
        tokio::select! {
            Some(msg) = incoming.recv() => match msg {
                Incoming::Note(n) => d.show(*n).await,
                Incoming::CloseRequest(id) => d.close(id, CLOSED_BY_CALL).await,
            },
            Some(n) = internal.recv() => {
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                if let Some(h) = n.on_action {
                    d.handlers.insert(id, h);
                }
                d.show(Note {
                    id,
                    app: "lintel".into(),
                    summary: n.summary,
                    body: n.body,
                    icon: n.icon,
                    image: None,
                    actions: n.actions,
                    urgency: n.urgency,
                    timeout: match n.timeout_ms { 0 => None, t if t > 0 => Some(Duration::from_millis(t as u64)), _ => Some(Duration::ZERO) },
                    value: None,
                    desktop_entry: String::new(),
                    transient: false,
                    tag: String::new(),
                    time: now(),
                }).await;
            }
            Some(ev) = events.recv() => d.event(ev).await,
            Some((cmd, reply)) = commands.recv() => {
                let answer = d.command(&cmd).await;
                let _ = reply.send(answer);
            }
            _ = tokio::time::sleep_until(next.unwrap_or_else(Instant::now).into()), if next.is_some() => {
                let now = Instant::now();
                let due: Vec<u32> = d.deadlines.iter().filter(|(_, t)| **t <= now).map(|(id, _)| *id).collect();
                for id in due {
                    d.close(id, EXPIRED).await;
                }
            }
        }
    }
}

async fn serve(tx: mpsc::UnboundedSender<Incoming>) -> Result<Connection> {
    let conn = Connection::session().await?;
    conn.object_server().at(PATH, Server { tx }).await?;
    conn.request_name("org.freedesktop.Notifications")
        .await
        .context("owning org.freedesktop.Notifications")?;
    Ok(conn)
}

impl Daemon {
    fn cfg(&self) -> &crate::config::NotificationsConfig {
        &self.hub.cfg.notifications
    }

    /// The urgency's timeout, for a request that left it to us.
    fn default_timeout(&self, urgency: u8) -> Option<Duration> {
        let secs = match urgency {
            0 => self.cfg().timeout_low,
            2 => self.cfg().timeout_critical,
            _ => self.cfg().timeout_normal,
        };
        (secs > 0).then(|| Duration::from_secs(secs))
    }

    async fn show(&mut self, mut n: Note) {
        if n.timeout == Some(Duration::ZERO) {
            n.timeout = self.default_timeout(n.urgency);
        }
        if n.icon.is_empty() && !n.desktop_entry.is_empty() {
            if let Some(i) = self.entry_icons.get(&n.desktop_entry.to_lowercase()) {
                n.icon = i.clone();
            }
        }
        if n.icon.is_empty() && n.image.is_none() {
            // the app's name as an icon name, if the theme has one
            n.icon = n.app.to_lowercase().replace(' ', "-");
        }
        // a stack tag replaces the app's earlier one with the same tag
        if !n.tag.is_empty() {
            if let Some(prev) = self
                .visible
                .iter()
                .chain(self.queue.iter())
                .find(|p| p.app == n.app && p.tag == n.tag && p.id != n.id)
                .map(|p| p.id)
            {
                self.remove(prev);
                self.hub.notify(NotifyUpdate::Close(prev));
            }
        }
        if self.paused {
            self.remember(&n);
            self.publish();
            return;
        }
        if let Some(slot) = self.visible.iter_mut().find(|v| v.id == n.id) {
            // replaced in place
            *slot = n.clone();
            self.arm(&n);
            self.hub.notify(NotifyUpdate::Show(Box::new(n)));
            return;
        }
        if self.visible.len() >= self.cfg().max_visible {
            self.queue.retain(|q| q.id != n.id);
            self.queue.push_back(n);
            return;
        }
        self.arm(&n);
        self.visible.push(n.clone());
        self.hub.notify(NotifyUpdate::Show(Box::new(n)));
        self.publish();
    }

    fn arm(&mut self, n: &Note) {
        match n.timeout {
            Some(t) => {
                self.deadlines.insert(n.id, Instant::now() + t);
            }
            None => {
                self.deadlines.remove(&n.id);
            }
        }
    }

    fn remove(&mut self, id: u32) -> Option<Note> {
        self.deadlines.remove(&id);
        if let Some(i) = self.visible.iter().position(|v| v.id == id) {
            return Some(self.visible.remove(i));
        }
        if let Some(i) = self.queue.iter().position(|v| v.id == id) {
            return self.queue.remove(i);
        }
        None
    }

    fn remember(&mut self, n: &Note) {
        if n.transient {
            return;
        }
        self.history.retain(|h| h.id != n.id);
        self.history.push_front(n.clone());
        while self.history.len() > self.cfg().history {
            self.history.pop_back();
        }
    }

    async fn close(&mut self, id: u32, reason: u32) {
        let Some(n) = self.remove(id) else { return };
        self.remember(&n);
        self.handlers.remove(&id);
        self.hub.notify(NotifyUpdate::Close(id));
        self.signal_closed(id, reason).await;
        // room for the next one waiting
        if let Some(next) = self.queue.pop_front() {
            self.arm(&next);
            self.visible.push(next.clone());
            self.hub.notify(NotifyUpdate::Show(Box::new(next)));
        }
        self.publish();
    }

    async fn signal_closed(&self, id: u32, reason: u32) {
        if let Ok(iface) = self.conn.object_server().interface::<_, Server>(PATH).await {
            let _ = Server::notification_closed(iface.signal_emitter(), id, reason).await;
        }
    }

    async fn invoke(&self, id: u32, key: &str) {
        if let Some(h) = self.handlers.get(&id) {
            h(key);
            return;
        }
        if let Ok(iface) = self.conn.object_server().interface::<_, Server>(PATH).await {
            let _ = Server::action_invoked(iface.signal_emitter(), id, key.to_string()).await;
        }
    }

    async fn event(&mut self, ev: NotifyEvent) {
        match ev {
            NotifyEvent::Clicked(id) => {
                let Some(n) = self.visible.iter().find(|v| v.id == id).cloned() else {
                    return;
                };
                if n.actions.iter().any(|(k, _)| k == "default") {
                    self.invoke(id, "default").await;
                }
                self.close(id, DISMISSED).await;
                // the app's window: its workspace, out of the scratchpad
                let hub = self.hub.clone();
                tokio::spawn(async move {
                    if let Err(e) = focus_app(&n, &hub).await {
                        eprintln!("notifications: focusing {}: {e}", n.app);
                    }
                });
            }
            NotifyEvent::Action(id, key) => {
                self.invoke(id, &key).await;
                self.close(id, DISMISSED).await;
            }
            NotifyEvent::Dismiss(id) => self.close(id, DISMISSED).await,
            NotifyEvent::DismissAll => {
                let ids: Vec<u32> = self.visible.iter().map(|v| v.id).collect();
                for id in ids {
                    self.close(id, DISMISSED).await;
                }
                self.queue.clear();
            }
        }
    }

    async fn command(&mut self, cmd: &str) -> String {
        match cmd.trim() {
            "close" => {
                if let Some(id) = self.visible.last().map(|v| v.id) {
                    self.close(id, DISMISSED).await;
                }
                "ok".into()
            }
            "close-all" => {
                self.event(NotifyEvent::DismissAll).await;
                "ok".into()
            }
            "action" => {
                if let Some(id) = self.visible.last().map(|v| v.id) {
                    self.event(NotifyEvent::Clicked(id)).await;
                }
                "ok".into()
            }
            "pop" => {
                if let Some(n) = self.history.pop_front() {
                    self.show_again(n).await;
                }
                "ok".into()
            }
            "history" => {
                let items: Vec<picker::Item> = self
                    .history
                    .iter()
                    .map(|n| picker::Item {
                        label: format!("{}  {}", wall_time(n.time), plain(&n.summary)),
                        detail: plain(&n.body),
                        icon: n.icon.clone(),
                        key: n.app.clone(),
                        extra: n.app.clone(),
                        comment: String::new(),
                        parent: None,
                    })
                    .collect();
                if items.is_empty() {
                    return "no history".into();
                }
                match self.hub.pick("notifications", "Notifications", "", items).await {
                    picker::Choice::Picked(i) => {
                        if let Some(n) = self.history.remove(i) {
                            self.show_again(n).await;
                        }
                        "ok".into()
                    }
                    _ => String::new(),
                }
            }
            "clear" => {
                self.history.clear();
                self.publish();
                "ok".into()
            }
            "toggle" => {
                self.paused = !self.paused;
                self.publish();
                if self.paused {
                    "paused".into()
                } else {
                    "resumed".into()
                }
            }
            "status" => self.status().to_string(),
            // as JSON, for `lintel mcp`: on screen and in the history,
            // newest first, without opening anything
            "list" => {
                let note = |n: &Note| {
                    json!({
                        "id": n.id, "time": n.time, "clock": wall_time(n.time), "app": n.app,
                        "summary": plain(&n.summary), "body": plain(&n.body), "urgency": n.urgency,
                        "actions": n.actions.iter().map(|(_, label)| label).collect::<Vec<_>>(),
                    })
                };
                json!({
                    "visible": self.visible.iter().rev().map(note).collect::<Vec<_>>(),
                    "history": self.history.iter().map(note).collect::<Vec<_>>(),
                    "paused": self.paused,
                })
                .to_string()
            }
            _ => "error: close | close-all | action | pop | history | clear | toggle | status | list".into(),
        }
    }

    /// Back from the history, with its id, for the urgency's time.
    async fn show_again(&mut self, mut n: Note) {
        n.timeout = Some(Duration::ZERO);
        let paused = self.paused;
        self.paused = false;
        self.show(n).await;
        self.paused = paused;
    }

    fn status(&self) -> serde_json::Value {
        let (icon, class) = if self.paused {
            ('\u{f009b}', "paused")
        } else if !self.history.is_empty() {
            ('\u{f009a}', "history")
        } else {
            ('\u{f009c}', "empty")
        };
        let mut info = match self.history.front() {
            Some(n) => format!("{}  {}: {}", wall_time(n.time), plain(&n.summary), plain(&n.body)),
            None => "No notifications".into(),
        };
        if self.history.len() > 1 {
            info += &format!("\n{} more", self.history.len() - 1);
        }
        if self.paused {
            info = format!("Silenced\n{info}");
        }
        json!({
            "text": icon.to_string(), "info": info, "class": class,
            "count": self.history.len(), "visible": self.visible.len(), "paused": self.paused,
        })
    }

    fn publish(&self) {
        self.hub.emit("notifications", self.status());
    }
}

fn wall_time(unix: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(unix as i64, 0)
        .single()
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// Markup tags out, whitespace collapsed.
pub fn plain(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The window of the app that sent a notification: by its desktop entry
/// (the app id sway sees is usually that), else by its name.
async fn focus_app(n: &Note, hub: &Hub) -> Result<()> {
    let windows = picker::windows().await?;
    let entry = n.desktop_entry.trim_end_matches(".desktop").to_lowercase();
    let name = n.app.to_lowercase();
    let first_word = name.split_whitespace().next().unwrap_or("").to_string();
    let found = windows
        .iter()
        .find(|w| !entry.is_empty() && w.app.to_lowercase() == entry)
        .or_else(|| windows.iter().find(|w| !entry.is_empty() && w.app.to_lowercase().contains(&entry)))
        .or_else(|| windows.iter().find(|w| !name.is_empty() && w.app.to_lowercase() == name))
        .or_else(|| {
            windows
                .iter()
                .find(|w| first_word.len() > 2 && w.app.to_lowercase().contains(&first_word))
        })
        .or_else(|| {
            windows
                .iter()
                .find(|w| first_word.len() > 2 && w.title.to_lowercase().contains(&first_word))
        });
    match found {
        Some(w) => picker::focus(w, hub).await,
        None => Ok(()),
    }
}
