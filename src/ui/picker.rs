//! The picker window: a layer-shell window in the middle of the screen
//! with the keyboard, a field and the matching lines under it. Typing
//! filters (fuzzy), Up/Down/Tab move, Enter picks, Escape cancels,
//! Ctrl+Tab asks for the other list. The order is by frecency: how often
//! and how recently a line was picked, remembered per list in
//! ~/.local/state/lintel/frecency.json; while typing, the match score
//! leads and the frecency pushes (see `score`). The match ignores case:
//! "Teleg" and "teleg" find the same lines.

use crate::config::Config;
use crate::sources::picker::{Choice, Item, PickRequest};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
struct Use {
    count: u32,
    last: u64,
}

/// The frecency file: "list\u{1f}key" to its uses.
#[derive(Default, Serialize, Deserialize)]
struct Frecency(HashMap<String, Use>);

impl Frecency {
    fn path() -> PathBuf {
        let dir = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"))
            .join("lintel");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("frecency.json")
    }
    fn load() -> Frecency {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }
    fn save(&self) {
        if let Ok(t) = serde_json::to_string(self) {
            let _ = std::fs::write(Self::path(), t);
        }
    }
    /// Uses, fading with a half-life of thirty days.
    fn rank(&self, list: &str, key: &str) -> f64 {
        let Some(u) = self.0.get(&format!("{list}\u{1f}{key}")) else {
            return 0.0;
        };
        let days = now().saturating_sub(u.last) as f64 / 86400.0;
        u.count as f64 * (0.5f64).powf(days / 30.0)
    }
    fn used(&mut self, list: &str, key: &str) {
        let u = self.0.entry(format!("{list}\u{1f}{key}")).or_default();
        u.count += 1;
        u.last = now();
        self.save();
    }
}

/// What a key means in a list. Every list lintel shows, this one and
/// the ones to come, reads its keys here, so they all move the same way:
/// Down, Tab, Ctrl+N and Ctrl+K go down; Up, Shift+Tab, Ctrl+P and
/// Ctrl+L go up; Page keys jump; Enter picks; Escape and Ctrl+G cancel;
/// Ctrl+Tab, Ctrl+J and Ctrl+; ask for the other list. The letters are
/// the owner's arrows: j left, k down, l up, ; right.
pub enum ListKey {
    Move(i32),
    Pick,
    Cancel,
    Switch,
    Other,
}

pub fn list_key(key: gdk::Key, state: gdk::ModifierType) -> ListKey {
    use gdk::Key as K;
    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    if ctrl {
        return match key {
            K::Tab | K::ISO_Left_Tab | K::j | K::J | K::semicolon => ListKey::Switch,
            K::n | K::N | K::k | K::K => ListKey::Move(1),
            K::p | K::P | K::l | K::L => ListKey::Move(-1),
            K::g | K::G => ListKey::Cancel,
            _ => ListKey::Other,
        };
    }
    match key {
        K::Escape => ListKey::Cancel,
        K::Down | K::Tab => ListKey::Move(1),
        K::Up | K::ISO_Left_Tab => ListKey::Move(-1),
        K::Page_Down => ListKey::Move(5),
        K::Page_Up => ListKey::Move(-5),
        K::Return | K::KP_Enter => ListKey::Pick,
        _ => ListKey::Other,
    }
}

/// How well a line matches what was typed, None when it does not.
///
/// The line's own name counts most, and more when the typing is its
/// start ("Teleg" in "Telegram") or a word's start ("code" in "Visual
/// Studio Code"); then the generic name and the keywords, the comment
/// last: "mail" finds the mail client before an app whose comment spells
/// it out. A window under an app is a title, not a name: it weighs like
/// the detail, so a browser tab that mentions Telegram stays below
/// Telegram itself. The frecency pushes, softly enough that a name
/// typed out beats a much used app that only matches loosely.
fn score(matcher: &SkimMatcherV2, text: &str, item: &Item, rank: f64) -> Option<f64> {
    let frecency = 20.0 * (1.0 + rank).ln();
    if text.is_empty() {
        return Some(frecency);
    }
    let field = |s: &str, bonus: f64| matcher.fuzzy_match(s, text).map(|m| m as f64 + bonus);
    let name = if item.parent.is_some() {
        field(&item.label, 40.0)
    } else {
        field(&item.label, 100.0).map(|m| m + start_bonus(&item.label, text))
    };
    let best = [name, field(&item.detail, 60.0), field(&item.extra, 60.0), field(&item.comment, 0.0)]
        .into_iter()
        .flatten()
        .reduce(f64::max)?;
    Some(best + frecency)
}

/// The typing as the name's start, or a word's start in it.
fn start_bonus(name: &str, text: &str) -> f64 {
    let (name, text) = (name.to_lowercase(), text.trim().to_lowercase());
    if text.is_empty() {
        0.0
    } else if name.starts_with(&text) {
        150.0
    } else if name.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&text)) {
        100.0
    } else {
        0.0
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

struct Open {
    request: PickRequest,
    /// Indexes into the request's items, in shown order.
    shown: Vec<usize>,
    selected: usize,
    /// The first shown item on screen: `rows` lines from here are drawn.
    top: usize,
}

pub struct Picker {
    cfg: Arc<Config>,
    window: gtk::Window,
    prompt: gtk::Label,
    entry: gtk::Entry,
    list: gtk::Box,
    open: RefCell<Option<Open>>,
    frecency: RefCell<Frecency>,
    matcher: SkimMatcherV2,
    /// Set while the entry's text is changed by us, not the user.
    quiet: Cell<bool>,
    /// Where the pointer last was over the lines: a hover selects only
    /// when it moves, not when the lines change under a still pointer
    /// (the arrows would be pulled back to it).
    pointer: Cell<Option<(f64, f64)>>,
}

impl Picker {
    pub fn new(cfg: Arc<Config>) -> Rc<Picker> {
        let window = gtk::Window::new();
        window.set_title(Some("lintel picker"));
        window.add_css_class("lintel-picker");
        window.set_decorated(false);
        window.init_layer_shell();
        window.set_namespace(Some("lintel-picker"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::Exclusive);
        window.set_exclusive_zone(-1);
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("picker");
        root.set_size_request(cfg.picker.width, -1);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        head.add_css_class("head");
        let prompt = gtk::Label::new(None);
        prompt.add_css_class("prompt");
        let entry = gtk::Entry::new();
        entry.set_hexpand(true);
        head.append(&prompt);
        head.append(&entry);
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("rows");
        root.append(&head);
        root.append(&list);
        window.set_child(Some(&root));

        let picker = Rc::new(Picker {
            cfg,
            window: window.clone(),
            prompt,
            entry: entry.clone(),
            list: list.clone(),
            open: RefCell::new(None),
            frecency: RefCell::new(Frecency::load()),
            matcher: SkimMatcherV2::default().ignore_case(),
            quiet: Cell::new(false),
            pointer: Cell::new(None),
        });

        let p = picker.clone();
        entry.connect_changed(move |_| {
            if !p.quiet.get() {
                p.refilter();
            }
        });
        let p = picker.clone();
        entry.connect_activate(move |_| p.pick());
        let p = picker.clone();
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.connect_key_pressed(move |_, key, _, state| {
            match list_key(key, state) {
                ListKey::Cancel => p.finish(Choice::Cancelled),
                ListKey::Switch => p.finish(Choice::Switch),
                ListKey::Move(by) => p.step(by),
                ListKey::Pick => p.pick(),
                ListKey::Other => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        window.add_controller(keys);
        // the wheel scrolls the lines, a notch a line; the selection
        // stays on screen
        let p = picker.clone();
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::DISCRETE);
        wheel.connect_scroll(move |_, _, dy| {
            p.scroll(dy as i32);
            glib::Propagation::Stop
        });
        list.add_controller(wheel);
        // the mouse on the list, not on each line: the lines are redrawn
        // on every change, and a controller on a line gone mid-event is
        // what GTK complained about. A click picks the line under it; a
        // move selects it, so the mouse and the arrows agree on what
        // Enter picks. The first event after opening only takes note of
        // where the pointer is.
        let p = picker.clone();
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(move |_, x, y| {
            let moved = p.pointer.get().is_some_and(|last| last != (x, y));
            p.pointer.set(Some((x, y)));
            if moved {
                p.hover(x, y);
            }
        });
        list.add_controller(motion);
        let p = picker.clone();
        let click = gtk::GestureClick::new();
        click.connect_pressed(move |_, _, x, y| {
            if p.line_at(x, y).is_some() {
                p.hover(x, y);
                p.pick();
            }
        });
        list.add_controller(click);
        // the window stays, hidden, between uses: mapping is the slow part
        picker
    }

    /// Show a list. One already open is cancelled first; when it is the
    /// same list (the launcher's key pressed again) that is all: a toggle.
    pub fn show(self: &Rc<Self>, request: PickRequest) {
        if let Some(prev) = self.open.borrow_mut().take() {
            let same = prev.request.name == request.name;
            let _ = prev.request.reply.send(Choice::Cancelled);
            if same {
                let _ = request.reply.send(Choice::Cancelled);
                self.window.set_visible(false);
                return;
            }
        }
        self.quiet.set(true);
        self.entry.set_text(&request.query);
        self.prompt.set_text(&request.prompt);
        self.prompt.set_visible(!request.prompt.is_empty());
        self.quiet.set(false);
        self.pointer.set(None);
        *self.open.borrow_mut() = Some(Open {
            request,
            shown: Vec::new(),
            selected: 0,
            top: 0,
        });
        self.refilter();
        self.window.present();
        self.entry.grab_focus();
        // the focus selects the text; typing must add to it, not replace it
        let end = self.entry.text().chars().count() as i32;
        self.entry.select_region(end, end);
    }

    /// `lintel cancel`.
    pub fn cancel(&self) {
        self.finish(Choice::Cancelled);
    }

    fn finish(&self, choice: Choice) {
        let Some(open) = self.open.borrow_mut().take() else { return };
        if let Choice::Picked(i) = &choice {
            if let Some(item) = open.request.items.get(*i) {
                self.frecency.borrow_mut().used(&open.request.name, &item.key);
            }
        }
        let _ = open.request.reply.send(choice);
        self.window.set_visible(false);
    }

    fn pick(&self) {
        let choice = {
            let open = self.open.borrow();
            open.as_ref().and_then(|o| o.shown.get(o.selected).copied())
        };
        match choice {
            Some(i) => self.finish(Choice::Picked(i)),
            None => self.finish(Choice::Cancelled),
        }
    }

    fn step(self: &Rc<Self>, by: i32) {
        {
            let mut open = self.open.borrow_mut();
            let Some(o) = open.as_mut() else { return };
            if o.shown.is_empty() {
                return;
            }
            let n = o.shown.len() as i32;
            o.selected = ((o.selected as i32 + by).rem_euclid(n)) as usize;
            // the view follows the selection
            let rows = self.cfg.picker.rows.max(1);
            if o.selected < o.top {
                o.top = o.selected;
            } else if o.selected >= o.top + rows {
                o.top = o.selected + 1 - rows;
            }
        }
        self.redraw();
    }

    /// The shown item under a point of the list, if any.
    fn line_at(&self, x: f64, y: f64) -> Option<usize> {
        let mut w = self.list.pick(x, y, gtk::PickFlags::DEFAULT)?;
        while w.parent().as_ref() != Some(self.list.upcast_ref()) {
            w = w.parent()?;
        }
        let mut index = 0;
        let mut prev = w.prev_sibling();
        while let Some(s) = prev {
            index += 1;
            prev = s.prev_sibling();
        }
        let top = self.open.borrow().as_ref()?.top;
        Some(top + index)
    }

    /// Select the line under the pointer.
    fn hover(self: &Rc<Self>, x: f64, y: f64) {
        let Some(pos) = self.line_at(x, y) else { return };
        {
            let mut open = self.open.borrow_mut();
            let Some(o) = open.as_mut() else { return };
            if o.selected == pos || pos >= o.shown.len() {
                return;
            }
            o.selected = pos;
        }
        self.redraw();
    }

    /// The wheel: the view moves `by` lines, and the selection with it
    /// when it would leave the screen.
    fn scroll(self: &Rc<Self>, by: i32) {
        {
            let mut open = self.open.borrow_mut();
            let Some(o) = open.as_mut() else { return };
            let rows = self.cfg.picker.rows.max(1);
            let last = o.shown.len().saturating_sub(rows);
            let top = (o.top as i32 + by).clamp(0, last as i32) as usize;
            if top == o.top {
                return;
            }
            o.top = top;
            o.selected = o.selected.clamp(top, top + rows - 1);
        }
        self.redraw();
    }

    /// The items that match the text, best first: the fuzzy score, and
    /// the frecency as a push (an empty text is frecency alone). An item
    /// and the items under it (an app and its windows) are one group: it
    /// scores as its best member and shows whole, the parent first.
    fn refilter(self: &Rc<Self>) {
        let text = self.entry.text().to_string();
        {
            let mut open = self.open.borrow_mut();
            let Some(o) = open.as_mut() else { return };
            let frecency = self.frecency.borrow();
            let items = &o.request.items;
            let score_of = |item: &Item| score(&self.matcher, &text, item, frecency.rank(&o.request.name, &item.key));
            let mut groups: Vec<(f64, usize)> = Vec::new();
            let mut best: HashMap<usize, f64> = HashMap::new();
            for (i, item) in items.iter().enumerate() {
                let Some(s) = score_of(item) else { continue };
                let g = item.parent.unwrap_or(i);
                match best.get_mut(&g) {
                    Some(b) => *b = b.max(s),
                    None => {
                        best.insert(g, s);
                        groups.push((0.0, g));
                    }
                }
            }
            for g in groups.iter_mut() {
                g.0 = best[&g.1];
            }
            // stable: equal scores keep the list's order
            groups.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            // each group whole, in the list's order: one pass, not one per
            // group, now that the list is not cut at `rows`
            let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
            for (i, item) in items.iter().enumerate() {
                members.entry(item.parent.unwrap_or(i)).or_default().push(i);
            }
            let mut shown = Vec::new();
            for (_, g) in groups {
                shown.extend(members.remove(&g).unwrap_or_default());
            }
            o.shown = shown;
            o.selected = 0;
            o.top = 0;
        }
        self.redraw();
    }

    fn redraw(self: &Rc<Self>) {
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        let open = self.open.borrow();
        let Some(o) = open.as_ref() else { return };
        let rows = self.cfg.picker.rows.max(1);
        for (pos, &i) in o.shown.iter().enumerate().skip(o.top).take(rows) {
            let item: &Item = &o.request.items[i];
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.add_css_class("row");
            if pos == o.selected {
                row.add_css_class("selected");
            }
            if item.parent.is_some() {
                row.add_css_class("child");
            }
            if !item.icon.is_empty() {
                row.append(&super::icon_image(&item.icon, self.cfg.picker.icon_size));
            }
            let label = gtk::Label::new(Some(&item.label));
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            row.append(&label);
            if !item.detail.is_empty() {
                let detail = gtk::Label::new(Some(&item.detail));
                detail.add_css_class("detail");
                detail.set_xalign(0.0);
                detail.set_hexpand(true);
                detail.set_ellipsize(gtk::pango::EllipsizeMode::End);
                row.append(&detail);
            }
            self.list.append(&row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::score;
    use crate::sources::picker::Item;
    use fuzzy_matcher::skim::SkimMatcherV2;
    use fuzzy_matcher::FuzzyMatcher;

    fn item(label: &str, detail: &str, parent: Option<usize>) -> Item {
        Item {
            label: label.into(),
            detail: detail.into(),
            icon: String::new(),
            key: String::new(),
            extra: String::new(),
            comment: String::new(),
            parent,
        }
    }

    /// "Teleg" puts Telegram first, open in its single window, above a
    /// much more used browser whose open tab is about Telegram, and above
    /// apps whose names hold the letters scattered.
    #[test]
    fn name_start_leads() {
        let m = SkimMatcherV2::default().ignore_case();
        for q in ["Teleg", "teleg", "tele"] {
            let telegram = score(&m, q, &item("Telegram  [3] Telegram (907)", "Messaging", None), 11.0).unwrap();
            let tab = item("[2] telegram shortcut for focus chat panel - Google Search - Chromium", "", Some(0));
            let tab = score(&m, q, &tab, 35.0).unwrap();
            assert!(telegram > tab + 50.0, "{q}: {telegram} vs {tab}");
            for other in ["Text Editor", "Telephony Settings", "Teams for Linux"] {
                if let Some(s) = score(&m, q, &item(other, "", None), 0.0) {
                    assert!(telegram > s, "{q}: {telegram} vs {other} {s}");
                }
            }
        }
    }

    /// A word in the generic name beats the same letters scattered in a
    /// comment.
    #[test]
    fn fields_weigh() {
        let m = SkimMatcherV2::default();
        let generic = m.fuzzy_match("Mail Client", "mail").unwrap() + 60;
        let comment = m.fuzzy_match("Transfer files from/to MTP devices", "mail").map(|s| s).unwrap_or(0);
        assert!(generic > comment + 50, "{generic} vs {comment}");
    }
}
