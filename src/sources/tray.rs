//! The system tray: a StatusNotifierWatcher of our
//! own on the session bus (unless one is already there, then we use it),
//! a StatusNotifierHost that follows every registered item, and each
//! item's properties re-read on its signals (NewIcon, NewStatus, ...).
//! The icons and the menus are drawn on the GTK side (ui/tray.rs); a
//! menu's layout is read here from the item's dbusmenu object when the
//! icon is clicked, and the chosen entry's "clicked" event sent back.
//! Nothing polls: items announce themselves and their changes, the bus
//! tells us when one goes away.

use super::Hub;
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

#[derive(Debug, Clone)]
pub enum Icon {
    None,
    /// A themed icon name, or an absolute file path.
    Name(String),
    /// Premultiplied-free RGBA bytes, row-major.
    Pixmap {
        width: i32,
        height: i32,
        rgba: Vec<u8>,
    },
}

impl Icon {
    pub fn is_none(&self) -> bool {
        matches!(self, Icon::None)
    }
}

#[derive(Debug, Clone)]
pub struct TrayItem {
    /// "busname/path", the watcher's key.
    pub key: String,
    pub id: String,
    pub title: String,
    /// Active, Passive (hidden) or NeedsAttention.
    pub status: String,
    pub icon: Icon,
    pub attention_icon: Icon,
    pub tooltip: String,
    /// The item only has a menu: left click opens it.
    pub item_is_menu: bool,
    /// The dbusmenu object path, when the item has one.
    pub menu: Option<String>,
    /// An extra icon theme directory.
    pub icon_theme_path: String,
}

#[derive(Debug, Clone)]
pub enum TrayUpdate {
    Item(Box<TrayItem>),
    Remove(String),
    /// The item's menu, as asked with MenuOpen.
    Menu {
        key: String,
        root: MenuNode,
    },
}

/// One entry of a dbusmenu layout.
#[derive(Debug, Clone, Default)]
pub struct MenuNode {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    pub visible: bool,
    pub separator: bool,
    /// "checkmark", "radio" or empty.
    pub toggle_type: String,
    /// 1 on, 0 off, -1 indeterminate.
    pub toggle_state: i32,
    pub children: Vec<MenuNode>,
}

/// What the mouse does on an icon; the GTK side sends these.
#[derive(Debug)]
pub enum TrayCommand {
    Activate {
        key: String,
        x: i32,
        y: i32,
    },
    SecondaryActivate {
        key: String,
        x: i32,
        y: i32,
    },
    ContextMenu {
        key: String,
        x: i32,
        y: i32,
    },
    Scroll {
        key: String,
        delta: i32,
        orientation: String,
    },
    /// Read the item's menu layout; it comes back as TrayUpdate::Menu.
    MenuOpen {
        key: String,
    },
    /// An entry of that menu was chosen.
    MenuEvent {
        key: String,
        id: i32,
    },
}

// ---- the watcher ---------------------------------------------------------

#[derive(Default)]
struct Watcher {
    items: Vec<String>,
    hosts: Vec<String>,
}

/// An item registers with its bus name (path /StatusNotifierItem), or
/// with a path (the bus name is the caller's); the key joins the two.
fn item_key(service: &str, sender: &str) -> String {
    if service.starts_with('/') {
        format!("{sender}{service}")
    } else if service.contains('/') {
        service.to_string()
    } else {
        format!("{service}/StatusNotifierItem")
    }
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    async fn register_status_notifier_item(
        &mut self,
        service: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        let key = item_key(service, &sender);
        if !self.items.contains(&key) {
            self.items.push(key.clone());
            Self::status_notifier_item_registered(&emitter, &key).await?;
            self.registered_status_notifier_items_changed(&emitter).await?;
        }
        Ok(())
    }

    async fn register_status_notifier_host(
        &mut self,
        service: &str,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        if !self.hosts.iter().any(|h| h == service) {
            self.hosts.push(service.to_string());
        }
        Self::status_notifier_host_registered(&emitter).await?;
        self.is_status_notifier_host_registered_changed(&emitter).await?;
        Ok(())
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items.clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(emitter: &SignalEmitter<'_>, service: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Items do not unregister; their bus name vanishing is the signal. The
/// stream is opened before the watcher name is taken, so an item that
/// registers and leaves at once (Telegram does, then comes back) is seen.
async fn reap_items(conn: Connection, mut stream: zbus::fdo::NameOwnerChangedStream) -> Result<()> {
    while let Some(sig) = stream.next().await {
        let args = sig.args()?;
        if args.new_owner().is_some() {
            continue;
        }
        drop_bus_name(&conn, &args.name().to_string()).await?;
    }
    bail!("NameOwnerChanged stream ended")
}

/// Forget every item of a bus name in our watcher (no-op when the
/// watcher is somebody else's: ours then holds no items).
async fn drop_bus_name(conn: &Connection, name: &str) -> Result<()> {
    let prefix = format!("{name}/");
    let iface = conn.object_server().interface::<_, Watcher>("/StatusNotifierWatcher").await?;
    let mut w = iface.get_mut().await;
    let gone: Vec<String> = w.items.iter().filter(|k| k.starts_with(&prefix)).cloned().collect();
    if gone.is_empty() {
        return Ok(());
    }
    w.items.retain(|k| !gone.contains(k));
    let emitter = iface.signal_emitter();
    for k in &gone {
        Watcher::status_notifier_item_unregistered(emitter, k).await?;
    }
    w.registered_status_notifier_items_changed(emitter).await?;
    Ok(())
}

// ---- the host ------------------------------------------------------------

pub async fn run(hub: Hub, mut cmds: mpsc::UnboundedReceiver<TrayCommand>) {
    loop {
        if let Err(e) = host(&hub, &mut cmds).await {
            eprintln!("tray: {e:#}; retrying in 5s");
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

struct Tracked {
    proxy: Proxy<'static>,
    task: JoinHandle<()>,
    hub: Hub,
}

async fn host(hub: &Hub, cmds: &mut mpsc::UnboundedReceiver<TrayCommand>) -> Result<()> {
    let conn = Connection::session().await?;
    conn.object_server().at("/StatusNotifierWatcher", Watcher::default()).await?;
    let owners = zbus::fdo::DBusProxy::new(&conn).await?.receive_name_owner_changed().await?;
    match conn.request_name("org.kde.StatusNotifierWatcher").await {
        Ok(()) => {
            let c = conn.clone();
            tokio::spawn(async move {
                if let Err(e) = reap_items(c, owners).await {
                    eprintln!("tray: {e:#}");
                }
            });
        }
        Err(e) => eprintln!("tray: another StatusNotifierWatcher owns the name ({e}); using it"),
    }
    let host_name = format!("org.kde.StatusNotifierHost-{}", std::process::id());
    let _ = conn.request_name(host_name.as_str()).await;

    let watcher: Proxy<'static> = zbus::proxy::Builder::new(&conn)
        .destination("org.kde.StatusNotifierWatcher")?
        .path("/StatusNotifierWatcher")?
        .interface("org.kde.StatusNotifierWatcher")?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let mut registered = watcher.receive_signal("StatusNotifierItemRegistered").await?;
    let mut unregistered = watcher.receive_signal("StatusNotifierItemUnregistered").await?;
    watcher
        .call_method("RegisterStatusNotifierHost", &(host_name.as_str(),))
        .await
        .context("registering as host")?;

    // an item whose bus name turns out to be gone reports itself here
    let (dead_tx, mut dead) = mpsc::unbounded_channel::<String>();
    let mut items: HashMap<String, Tracked> = HashMap::new();
    let initial: Vec<String> = watcher.get_property("RegisteredStatusNotifierItems").await.unwrap_or_default();
    for key in initial {
        add_item(&conn, hub, &mut items, key, &dead_tx).await;
    }
    publish(hub, &items);

    loop {
        tokio::select! {
            Some(msg) = registered.next() => {
                if let Ok(key) = msg.body().deserialize::<String>() {
                    add_item(&conn, hub, &mut items, key, &dead_tx).await;
                    publish(hub, &items);
                }
            }
            Some(msg) = unregistered.next() => {
                if let Ok(key) = msg.body().deserialize::<String>() {
                    if let Some(t) = items.remove(&key) {
                        t.task.abort();
                        hub.tray(TrayUpdate::Remove(key));
                        publish(hub, &items);
                    }
                }
            }
            Some(key) = dead.recv() => {
                if let Some(t) = items.remove(&key) {
                    t.task.abort();
                    hub.tray(TrayUpdate::Remove(key.clone()));
                    publish(hub, &items);
                    if let Some((bus, _)) = key.split_once('/') {
                        let _ = drop_bus_name(&conn, bus).await;
                    }
                }
            }
            cmd = cmds.recv() => {
                let Some(cmd) = cmd else { return Ok(()) };
                dispatch(&items, cmd).await;
            }
            else => bail!("the watcher went away"),
        }
    }
}

async fn add_item(conn: &Connection, hub: &Hub, items: &mut HashMap<String, Tracked>, key: String, dead: &mpsc::UnboundedSender<String>) {
    if items.contains_key(&key) {
        return;
    }
    let Some((bus, path)) = key.split_once('/') else { return };
    let proxy = zbus::proxy::Builder::new(conn)
        .destination(bus.to_string())
        .and_then(|b| b.path(format!("/{path}")))
        .and_then(|b| b.interface("org.kde.StatusNotifierItem"))
        .map(|b| b.cache_properties(CacheProperties::No));
    let proxy: Proxy<'static> = match proxy {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("tray: item {key}: {e}");
                return;
            }
        },
        Err(e) => {
            eprintln!("tray: item {key}: {e}");
            return;
        }
    };
    let task = tokio::spawn(watch_item(proxy.clone(), key.clone(), hub.clone(), dead.clone()));
    items.insert(
        key,
        Tracked {
            proxy,
            task,
            hub: hub.clone(),
        },
    );
}

/// `lintel get tray`: who is in the tray.
fn publish(hub: &Hub, items: &HashMap<String, Tracked>) {
    let mut keys: Vec<&String> = items.keys().collect();
    keys.sort();
    hub.emit("tray", json!(keys));
}

/// Follow one item: read everything now, and again on each of its
/// signals (a burst is one read).
async fn watch_item(proxy: Proxy<'static>, key: String, hub: Hub, dead: mpsc::UnboundedSender<String>) {
    let mut signals = match proxy.receive_all_signals().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tray: item {key}: {e}");
            return;
        }
    };
    loop {
        match fetch(&proxy, &key).await {
            Ok(item) => {
                // nothing answered: is anyone still there?
                if item.id.is_empty() && item.icon.is_none() && !name_has_owner(&proxy).await {
                    let _ = dead.send(key);
                    return;
                }
                hub.tray(TrayUpdate::Item(Box::new(item)));
            }
            Err(e) => eprintln!("tray: item {key}: {e}"),
        }
        if signals.next().await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(1), signals.next()).await {}
    }
}

async fn name_has_owner(proxy: &Proxy<'static>) -> bool {
    let Ok(dbus) = zbus::fdo::DBusProxy::new(proxy.connection()).await else {
        return true;
    };
    dbus.name_has_owner(proxy.destination().clone()).await.unwrap_or(true)
}

async fn fetch(proxy: &Proxy<'static>, key: &str) -> Result<TrayItem> {
    // one property at a time: items are sloppy, one broken property must
    // not hide the icon
    let get = |name: &'static str| async move { proxy.get_property::<OwnedValue>(name).await.ok() };
    let status = string(get("Status").await);
    let icon_name = string(get("IconName").await);
    let icon = if icon_name.is_empty() {
        pixmap(get("IconPixmap").await)
    } else {
        Icon::Name(icon_name)
    };
    let attention_name = string(get("AttentionIconName").await);
    let attention_icon = if attention_name.is_empty() {
        pixmap(get("AttentionIconPixmap").await)
    } else {
        Icon::Name(attention_name)
    };
    let menu = get("Menu")
        .await
        .and_then(|v| OwnedObjectPath::try_from(v).ok())
        .map(|p| p.to_string())
        .filter(|p| p != "/");
    Ok(TrayItem {
        key: key.to_string(),
        id: string(get("Id").await),
        title: string(get("Title").await),
        status: if status.is_empty() { "Active".into() } else { status },
        icon,
        attention_icon,
        tooltip: tooltip(get("ToolTip").await),
        item_is_menu: get("ItemIsMenu").await.and_then(|v| bool::try_from(v).ok()).unwrap_or(false),
        menu,
        icon_theme_path: string(get("IconThemePath").await),
    })
}

fn string(v: Option<OwnedValue>) -> String {
    v.and_then(|v| String::try_from(v).ok()).unwrap_or_default()
}

/// `a(iiay)`: ARGB32 in network byte order, several sizes. The smallest
/// one at least 32 px wide (two HiDPI icon sizes), else the largest.
fn pixmap(v: Option<OwnedValue>) -> Icon {
    let Some(v) = v else { return Icon::None };
    let Value::Array(arr) = &*v else { return Icon::None };
    let mut best: Option<(i32, i32, Vec<u8>)> = None;
    for e in arr.iter() {
        let Value::Structure(s) = e else { continue };
        let f = s.fields();
        if f.len() != 3 {
            continue;
        }
        let (Ok(w), Ok(h)) = (i32::try_from(f[0].clone()), i32::try_from(f[1].clone())) else {
            continue;
        };
        let Ok(bytes) = Vec::<u8>::try_from(f[2].clone()) else { continue };
        if w <= 0 || h <= 0 || bytes.len() != (w * h * 4) as usize {
            continue;
        }
        let better = match &best {
            None => true,
            Some((bw, _, _)) => {
                if *bw >= 32 {
                    w >= 32 && w < *bw
                } else {
                    w > *bw
                }
            }
        };
        if better {
            best = Some((w, h, bytes));
        }
    }
    match best {
        Some((width, height, argb)) => {
            let mut rgba = Vec::with_capacity(argb.len());
            for px in argb.as_chunks::<4>().0 {
                rgba.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
            }
            Icon::Pixmap { width, height, rgba }
        }
        None => Icon::None,
    }
}

/// `(s a(iiay) s s)`: icon name, icon pixmaps, title, description.
fn tooltip(v: Option<OwnedValue>) -> String {
    let Some(v) = v else { return String::new() };
    let Value::Structure(s) = &*v else { return String::new() };
    let f = s.fields();
    if f.len() != 4 {
        return String::new();
    }
    let title = String::try_from(f[2].clone()).unwrap_or_default();
    let body = String::try_from(f[3].clone()).unwrap_or_default();
    match (title.is_empty(), body.is_empty()) {
        (true, true) => String::new(),
        (false, true) => title,
        (true, false) => body,
        (false, false) => format!("{title}\n{body}"),
    }
}

/// The item's dbusmenu object, from its Menu property.
async fn menu_proxy(t: &Tracked) -> Result<Proxy<'static>> {
    let path: zbus::zvariant::OwnedObjectPath = t.proxy.get_property("Menu").await.context("the item has no Menu")?;
    Ok(zbus::proxy::Builder::new(t.proxy.connection())
        .destination(t.proxy.destination().to_owned())?
        .path(path)?
        .interface("com.canonical.dbusmenu")?
        .cache_properties(CacheProperties::No)
        .build()
        .await?)
}

/// A dbusmenu layout entry: `(ia{sv}av)`, the id, the properties, the
/// children (each a variant holding the same structure).
#[derive(serde::Deserialize, zbus::zvariant::Type)]
struct Layout(i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

fn parse_layout(l: &Layout) -> MenuNode {
    let mut n = MenuNode {
        id: l.0,
        enabled: true,
        visible: true,
        ..Default::default()
    };
    for (k, val) in &l.1 {
        let val: &Value = val;
        let val = match val {
            Value::Value(inner) => inner.as_ref(),
            v => v,
        };
        match k.as_str() {
            "label" => n.label = String::try_from(val.clone()).unwrap_or_default(),
            "enabled" => n.enabled = bool::try_from(val.clone()).unwrap_or(true),
            "visible" => n.visible = bool::try_from(val.clone()).unwrap_or(true),
            "type" => n.separator = String::try_from(val.clone()).map(|t| t == "separator").unwrap_or(false),
            "toggle-type" => n.toggle_type = String::try_from(val.clone()).unwrap_or_default(),
            "toggle-state" => n.toggle_state = i32::try_from(val.clone()).unwrap_or(-1),
            _ => {}
        }
    }
    n.children = l.2.iter().filter_map(|c| child_layout(c).map(|l| parse_layout(&l))).collect();
    n
}

/// A child variant back into a Layout: structure fields id, dict, array.
fn child_layout(v: &OwnedValue) -> Option<Layout> {
    let v: &Value = v;
    let v = match v {
        Value::Value(inner) => inner.as_ref(),
        v => v,
    };
    let Value::Structure(s) = v else { return None };
    let f = s.fields();
    if f.len() < 3 {
        return None;
    }
    let id = i32::try_from(f[0].clone()).ok()?;
    let props: HashMap<String, OwnedValue> = HashMap::try_from(f[1].clone()).ok()?;
    let children: Vec<OwnedValue> = match &f[2] {
        Value::Array(a) => a.iter().filter_map(|c| c.try_to_owned().ok()).collect(),
        _ => Vec::new(),
    };
    Some(Layout(id, props, children))
}

async fn dispatch(items: &HashMap<String, Tracked>, cmd: TrayCommand) {
    let (key, method, args): (&str, &str, (i32, i32)) = match &cmd {
        TrayCommand::Activate { key, x, y } => (key, "Activate", (*x, *y)),
        TrayCommand::SecondaryActivate { key, x, y } => (key, "SecondaryActivate", (*x, *y)),
        TrayCommand::ContextMenu { key, x, y } => (key, "ContextMenu", (*x, *y)),
        TrayCommand::Scroll { key, delta, orientation } => {
            if let Some(t) = items.get(key) {
                if let Err(e) = t.proxy.call_method("Scroll", &(*delta, orientation.as_str())).await {
                    eprintln!("tray: {key} Scroll: {e}");
                }
            }
            return;
        }
        TrayCommand::MenuOpen { key } => {
            let Some(t) = items.get(key) else { return };
            let result: Result<MenuNode> = async {
                let menu = menu_proxy(t).await?;
                // the app may build the menu now; its answer does not matter
                let _ = menu.call_method("AboutToShow", &(0i32,)).await;
                let reply = menu.call_method("GetLayout", &(0i32, -1i32, Vec::<&str>::new())).await?;
                let body = reply.body();
                let (_revision, layout): (u32, Layout) = body.deserialize()?;
                Ok(parse_layout(&layout))
            }
            .await;
            match result {
                Ok(root) => t.hub.tray(TrayUpdate::Menu { key: key.clone(), root }),
                Err(e) => eprintln!("tray: {key} menu: {e:#}"),
            }
            return;
        }
        TrayCommand::MenuEvent { key, id } => {
            if let Some(t) = items.get(key) {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as u32)
                    .unwrap_or(0);
                let result = async {
                    let menu = menu_proxy(t).await?;
                    menu.call_method("Event", &(*id, "clicked", Value::Str("".into()), stamp)).await?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(e) = result {
                    eprintln!("tray: {key} menu event: {e:#}");
                }
            }
            return;
        }
    };
    if let Some(t) = items.get(key) {
        if let Err(e) = t.proxy.call_method(method, &args).await {
            eprintln!("tray: {key} {method}: {e}");
        }
    }
}
