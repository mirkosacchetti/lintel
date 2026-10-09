//! The configuration: `~/.config/lintel/lintel.toml` and `style.css`.
//!
//! A bar is a list of *sources* (where values come from) and a list of
//! *modules* (what shows them). Nothing here polls: a source either listens
//! to a long-running program, or takes a snapshot when one of its
//! *triggers* fires (a udev event, a D-Bus signal, a netlink message, a
//! sway event, an IPC refresh, a hover), or is native (clock, pomodoro).

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub bar: Bar,
    #[serde(default)]
    pub pomodoro: PomodoroConfig,
    #[serde(default)]
    pub picker: PickerConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default, rename = "source")]
    pub sources: Vec<Source>,
    #[serde(default, rename = "module")]
    pub modules: Vec<Module>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Bar {
    /// Height of the bar in pixels.
    #[serde(default = "default_height")]
    pub height: i32,
    /// Width of the popup card in pixels (the CSS min-width should match).
    #[serde(default = "default_card_width")]
    pub card_width: i32,
    /// Exported to the commands as `$S`, so the config can say `$S/foo.sh`.
    #[serde(default)]
    pub scripts: String,
    /// Milliseconds the pointer must rest on a module before its card
    /// opens: a flick over the bar does not pop cards by.
    #[serde(default = "default_hover_delay")]
    pub hover_delay_ms: u64,
    /// Milliseconds a module stays open after the pointer leaves it: moving
    /// to the next module is a leave followed by an enter.
    #[serde(default = "default_leave_delay")]
    pub leave_delay_ms: u64,
    /// Milliseconds a command source waits after a trigger before it runs,
    /// so a burst of events (volume steps, a D-Bus flurry) is one snapshot.
    #[serde(default = "default_coalesce")]
    pub coalesce_ms: u64,
}

fn default_height() -> i32 {
    28
}
fn default_card_width() -> i32 {
    360
}
fn default_hover_delay() -> u64 {
    120
}
fn default_leave_delay() -> u64 {
    150
}
fn default_coalesce() -> u64 {
    100
}

impl Default for Bar {
    fn default() -> Self {
        Self {
            height: default_height(),
            card_width: default_card_width(),
            scripts: String::new(),
            hover_delay_ms: default_hover_delay(),
            leave_delay_ms: default_leave_delay(),
            coalesce_ms: default_coalesce(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PomodoroConfig {
    #[serde(default = "d25")]
    pub work_minutes: u64,
    #[serde(default = "d5")]
    pub short_break_minutes: u64,
    #[serde(default = "d15")]
    pub long_break_minutes: u64,
    /// A long break after this many pomodori.
    #[serde(default = "d4")]
    pub pomodori_until_long: u32,
    /// A session is this many pomodori: a notification celebrates it.
    #[serde(default = "d10")]
    pub pomodori_per_session: u32,
    /// "minutes" or "seconds": how the remaining time shows in the bar, and
    /// so how often the module wakes up (once a minute, or once a second).
    #[serde(default = "default_display")]
    pub display: String,
    /// Run when a phase ends (a bell, say). Runs through `sh -c`.
    #[serde(default)]
    pub on_phase_end: String,
    /// Icons per phase.
    #[serde(default = "default_icons")]
    pub icons: BTreeMap<String, String>,
}

fn d25() -> u64 {
    25
}
fn d5() -> u64 {
    5
}
fn d15() -> u64 {
    15
}
fn d4() -> u32 {
    4
}
fn d10() -> u32 {
    10
}
fn default_display() -> String {
    "minutes".into()
}
fn default_icons() -> BTreeMap<String, String> {
    // nerd font (material design): timer, coffee, leaf, pause, timer again,
    // timer-sand; monochrome like the rest of the bar
    [
        ("work", "\u{f051b}"),
        ("short_break", "\u{f0176}"),
        ("long_break", "\u{f032a}"),
        ("paused", "\u{f03e4}"),
        ("stopped", "\u{f051b}"),
        ("countdown", "\u{f051f}"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

impl Default for PomodoroConfig {
    fn default() -> Self {
        Self {
            work_minutes: d25(),
            short_break_minutes: d5(),
            long_break_minutes: d15(),
            pomodori_until_long: d4(),
            pomodori_per_session: d10(),
            display: default_display(),
            on_phase_end: String::new(),
            icons: default_icons(),
        }
    }
}

/// The notification daemon.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    /// Own org.freedesktop.Notifications and draw the notifications.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// "top-left", "top-center" or "top-right".
    #[serde(default = "default_position")]
    pub position: String,
    /// Pixels from the bar, and from the side.
    #[serde(default = "d16i")]
    pub offset: i32,
    #[serde(default = "default_notification_width")]
    pub width: i32,
    #[serde(default = "d8i")]
    pub gap: i32,
    #[serde(default = "d48i")]
    pub icon_size: i32,
    #[serde(default = "d5us")]
    pub max_visible: usize,
    #[serde(default = "d20us")]
    pub history: usize,
    /// Seconds per urgency; 0 is until closed.
    #[serde(default = "d4u")]
    pub timeout_low: u64,
    #[serde(default = "d6u")]
    pub timeout_normal: u64,
    #[serde(default)]
    pub timeout_critical: u64,
}

fn yes() -> bool {
    true
}
fn default_position() -> String {
    "top-center".into()
}
fn default_notification_width() -> i32 {
    420
}
fn d16i() -> i32 {
    16
}
fn d8i() -> i32 {
    8
}
fn d48i() -> i32 {
    48
}
fn d5us() -> usize {
    5
}
fn d20us() -> usize {
    20
}
fn d4u() -> u64 {
    4
}
fn d6u() -> u64 {
    6
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            position: default_position(),
            offset: d16i(),
            width: default_notification_width(),
            gap: d8i(),
            icon_size: d48i(),
            max_visible: d5us(),
            history: d20us(),
            timeout_low: d4u(),
            timeout_normal: d6u(),
            timeout_critical: 0,
        }
    }
}

/// The picker: the launcher, the window switcher and `lintel pick`.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PickerConfig {
    #[serde(default = "default_picker_width")]
    pub width: i32,
    /// How many lines show at most; the others scroll in.
    #[serde(default = "default_picker_rows")]
    pub rows: usize,
    #[serde(default = "default_picker_icon")]
    pub icon_size: i32,
    /// Runs the entries that want a terminal: the command, the entry's
    /// Exec appended.
    #[serde(default = "default_terminal")]
    pub terminal: String,
    /// Brings a scratchpad window up, `{id}` its con id; empty: sway's
    /// own focus does.
    #[serde(default)]
    pub scratchpad: String,
}

fn default_picker_width() -> i32 {
    560
}
fn default_picker_rows() -> usize {
    8
}
fn default_picker_icon() -> i32 {
    24
}
fn default_terminal() -> String {
    "alacritty -e".into()
}

impl Default for PickerConfig {
    fn default() -> Self {
        Self {
            width: default_picker_width(),
            rows: default_picker_rows(),
            icon_size: default_picker_icon(),
            terminal: default_terminal(),
            scratchpad: String::new(),
        }
    }
}

/// Where a variable's value comes from.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Source {
    /// The variable name the modules refer to.
    pub name: String,
    /// "listen": `command` runs for good and prints a JSON value (or a
    /// line of text) whenever something changes.
    /// "command": `command` runs once at start and again on every trigger,
    /// its output (JSON, or text) is the value.
    /// "clock": the time, formatted with `format` (text) and `info_format`
    /// (the card), refreshed exactly when the text changes.
    /// "sway": workspaces, binding mode and focused title over sway's IPC,
    /// in-process; fills `scratchpad` too (see sources/sway.rs).
    /// "display": the outputs over sway's IPC.
    /// "battery", "network", "bluetooth", "brightness", "powerprofile",
    /// "nightlight": native readers, run on `triggers` like a "command";
    /// "brightness" runs `command` for a screen without a backlight.
    /// The `notifications` variable comes from the daemon itself.
    /// "audio": the default output and input, from the sound server.
    /// "mpris": the media player, from playerctld and the sound server.
    /// "pomodoro": the pomodoro timer, driven over IPC.
    /// "timer": the one-shot countdown, driven over IPC.
    /// "static": `initial` is the value, forever (unless `lintel update`).
    pub kind: String,
    #[serde(default)]
    pub command: String,
    /// The value before the first one arrives: JSON, or a plain string.
    #[serde(default)]
    pub initial: Option<toml::Value>,
    /// For "command": what makes it run again. Each is `kind:argument`:
    ///   udev:SUBSYSTEM          a kernel uevent in that subsystem
    ///   dbus:BUS:NAME:PATH      a signal from NAME under PATH (BUS is
    ///                           session or system); for systemd the
    ///                           manager is subscribed to first
    ///   netlink                 a link, address or route change
    ///   sway:EVENT[,EVENT]      a sway IPC event (output, tick, ...)
    ///   refresh                 `lintel refresh NAME` (your own actions)
    ///   hover                   the module's card opens
    ///   timer:SECONDS           a plain interval (polling; here if you
    ///                           really need it, the example uses none)
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub info_format: String,
}

/// One thing in the bar.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Module {
    pub name: String,
    /// "left" or "right".
    #[serde(default = "default_side")]
    pub side: String,
    /// "module" (default: text with a card on hover), "list" (one button
    /// per item of `items`: workspaces, countdowns), "label" (bare text,
    /// no card), "tray" (the StatusNotifier icons), "calendar" (a module
    /// whose card also carries a month calendar, pinned to the right edge:
    /// ‹ and › or a scroll move a month, the month name returns to today;
    /// the names of months and days are the locale's).
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    /// Templates: `{var}`, `{var.key.0}`, `{var.key|trunc:60}`.
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub info: String,
    #[serde(default)]
    pub class: String,
    /// Pango markup in text and info.
    #[serde(default)]
    pub markup: bool,
    /// The text's width in the bar, at most, in characters: past it the
    /// text ends in an ellipsis, and when the bar is short of room the
    /// label gives up width before any other module does. Without it a
    /// label is as wide as its text and the bar grows past the screen.
    #[serde(default)]
    pub max_chars: Option<i32>,
    /// Hidden when the text equals this (the sway mode is "default").
    #[serde(default)]
    pub hide_when: Option<String>,
    /// The mouse: commands through `sh -c`, detached from the bar.
    #[serde(default)]
    pub click: String,
    #[serde(default)]
    pub middle: String,
    #[serde(default)]
    pub right: String,
    #[serde(default)]
    pub scroll_up: String,
    #[serde(default)]
    pub scroll_down: String,
    /// The card's action table: [badge, text] pairs, two per row.
    #[serde(default)]
    pub hints: Vec<[String; 2]>,
    /// For "list": the variable holding the array (`sway.workspaces`), and
    /// templates on each item's fields (`{name}`, `{focused}` ...).
    #[serde(default)]
    pub items: String,
    #[serde(default)]
    pub item_text: String,
    #[serde(default)]
    pub item_class: String,
    #[serde(default)]
    pub item_click: String,
    #[serde(default)]
    pub item_middle: String,
    #[serde(default)]
    pub item_right: String,
    /// A field and a button in the card (a custom pomodoro length, say).
    #[serde(default)]
    pub input: Option<Input>,
    /// Switches in the card, one row each (`[[module.toggles]]`).
    #[serde(default)]
    pub toggles: Vec<Toggle>,
    /// Rows of buttons in the card, under the switches
    /// (`[[module.buttons]]`).
    #[serde(default)]
    pub buttons: Vec<ButtonRow>,
    /// For "tray": the icons' size in pixels, and the gap between them.
    #[serde(default = "default_icon_size")]
    pub icon_size: i32,
    #[serde(default = "default_spacing")]
    pub spacing: i32,
}

fn default_icon_size() -> i32 {
    16
}
fn default_spacing() -> i32 {
    4
}

/// A value field, a label field and a button in a module's card:
/// `{value}` and `{label}` in the command are what was typed.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Input {
    /// The caption before the value field ("Minutes").
    #[serde(default)]
    pub prompt: String,
    /// What the value field holds before you type.
    #[serde(default)]
    pub default: String,
    /// The label field's placeholder; empty: no label field.
    #[serde(default)]
    pub label: String,
    /// What the label field holds before you type.
    #[serde(default)]
    pub label_default: String,
    #[serde(default = "default_button")]
    pub button: String,
    pub command: String,
}

/// A switch in a module's card: a name, a line under it, and the
/// command each way. The state is a template: off when it renders "",
/// "false", "0", "null" or "off", on otherwise; the switch shows it
/// again whenever its variables change, so a toggle from elsewhere shows.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Toggle {
    pub label: String,
    /// A template, the line under the name ("{nightlight.info}").
    #[serde(default)]
    pub detail: String,
    pub state: String,
    /// Run when the switch is turned on, and off.
    pub on: String,
    pub off: String,
}

/// A row of buttons in a module's card, with a caption over it. With a
/// `state` the row is a choice: the button whose `value` equals the
/// rendered state is marked active (the power profile in use), and the
/// mark follows the state's variables.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ButtonRow {
    #[serde(default)]
    pub label: String,
    /// A template.
    #[serde(default)]
    pub state: String,
    pub items: Vec<Button>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Button {
    pub text: String,
    pub command: String,
    #[serde(default)]
    pub value: String,
}

fn default_button() -> String {
    "Start".into()
}

fn default_side() -> String {
    "left".into()
}
fn default_kind() -> String {
    "module".into()
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config"))
        .join("lintel")
}

impl Config {
    pub fn load(path: Option<PathBuf>) -> Result<Config> {
        let path = path.unwrap_or_else(|| config_dir().join("lintel.toml"));
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        for m in &cfg.modules {
            if !matches!(m.kind.as_str(), "module" | "list" | "workspaces" | "label" | "tray" | "calendar") {
                anyhow::bail!("module {}: unknown kind {:?}", m.name, m.kind);
            }
            if !matches!(m.side.as_str(), "left" | "right") {
                anyhow::bail!("module {}: side must be left or right", m.name);
            }
        }
        for s in &cfg.sources {
            if !matches!(
                s.kind.as_str(),
                "listen"
                    | "command"
                    | "clock"
                    | "sway"
                    | "display"
                    | "audio"
                    | "mpris"
                    | "battery"
                    | "network"
                    | "bluetooth"
                    | "brightness"
                    | "powerprofile"
                    | "nightlight"
                    | "pomodoro"
                    | "timer"
                    | "static"
            ) {
                anyhow::bail!("source {}: unknown kind {:?}", s.name, s.kind);
            }
        }
        Ok(cfg)
    }

    /// The initial value of a source as JSON.
    pub fn initial_value(s: &Source) -> serde_json::Value {
        match &s.initial {
            Some(toml::Value::String(text)) => serde_json::from_str(text).unwrap_or(serde_json::Value::String(text.clone())),
            Some(v) => toml_to_json(v),
            None => serde_json::Value::String(String::new()),
        }
    }
}

fn toml_to_json(v: &toml::Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        toml::Value::String(s) => J::String(s.clone()),
        toml::Value::Integer(i) => J::from(*i),
        toml::Value::Float(f) => J::from(*f),
        toml::Value::Boolean(b) => J::Bool(*b),
        toml::Value::Datetime(d) => J::String(d.to_string()),
        toml::Value::Array(a) => J::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => J::Object(t.iter().map(|(k, v)| (k.clone(), toml_to_json(v))).collect()),
    }
}
