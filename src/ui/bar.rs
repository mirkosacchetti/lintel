//! One output's bar and its popup. The bar is a layer-shell window at
//! the top with the modules on the left and the right; the popup is a
//! second, transparent, full-width window right under it, mapped while a
//! card shows: hovering a module moves the card to sit under that module,
//! with the module's title, its information, its switches and buttons,
//! its input row and its mouse actions, and moving to the next module only moves the card. The
//! popup's input region is the card alone, so the windows below keep
//! their clicks.

use super::calendar::Calendar;
use super::store::Store;
use super::tray::Tray;
use crate::config::{Config, Module};
use crate::ipc::Request;
use crate::runner;
use crate::template::Template;
use gtk::cairo;
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

pub struct OutputUi {
    pub monitor: gdk::Monitor,
    pub id: u32,
    bar: gtk::Window,
    popup: gtk::Window,
}

impl OutputUi {
    pub fn close(&self, store: &Store) {
        store.drop_owner(self.id);
        self.popup.close();
        self.bar.close();
    }
}

/// The popup's card: a box of its own under the pointer's module, with
/// the title, the information, the input row (a label, an entry, a
/// button) for modules that declare one, and the action table. The
/// pointer moving from the module down into the card keeps it open,
/// leaving the card closes it. A card with an input takes the keyboard
/// as it opens, like the picker: the value is selected, so a digit
/// replaces it and Enter runs it as it is; closing the card (Enter,
/// Escape, the pointer leaving) gives the keyboard back to the window
/// that had it. Any other card takes it on demand, when clicked.
struct Card {
    popup: gtk::Window,
    fixed: gtk::Fixed,
    frame: gtk::Box,
    title: gtk::Label,
    info: gtk::Label,
    /// The month calendar, shown when the module's kind is "calendar".
    calendar: Rc<Calendar>,
    hints: gtk::Grid,
    input_row: gtk::Box,
    input_label: gtk::Label,
    entry: gtk::Entry,
    label_entry: gtk::Entry,
    button: gtk::Button,
    /// The input's command for the module shown now.
    input_command: RefCell<Option<String>>,
    /// The module's switches, a row each: the line under the name, and
    /// the switch.
    toggles: gtk::Box,
    toggle_rows: RefCell<Vec<(gtk::Label, gtk::Switch)>>,
    /// Set while a switch is moved to show a state, not by the user.
    quiet: Cell<bool>,
    /// The module's rows of buttons, over the switches: each button with
    /// its value.
    buttons: gtk::Box,
    button_rows: RefCell<Vec<Vec<(gtk::Button, String)>>>,
    scripts: String,
}

/// A switch's state template, rendered: on unless it says otherwise.
fn truthy(s: &str) -> bool {
    !matches!(s.trim(), "" | "false" | "0" | "null" | "off")
}

impl Card {
    fn new(popup: gtk::Window, width: i32, scripts: &str) -> Rc<Card> {
        // a fixed height: shrinking the surface to a pixel when the card
        // hides leaves GTK 4 without a redraw, and the old card on screen
        let fixed = gtk::Fixed::new();
        fixed.set_size_request(-1, 400);
        let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
        frame.add_css_class("card");
        frame.set_size_request(width, -1);
        frame.set_visible(false);
        let title = gtk::Label::new(None);
        title.add_css_class("card-title");
        title.set_halign(gtk::Align::Start);
        title.set_xalign(0.0);
        let info = gtk::Label::new(None);
        info.set_halign(gtk::Align::Start);
        info.set_xalign(0.0);
        info.set_max_width_chars(40);
        info.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let calendar = Calendar::new();
        let input_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        input_row.add_css_class("input");
        let input_label = gtk::Label::new(None);
        let entry = gtk::Entry::new();
        entry.set_width_chars(5);
        let label_entry = gtk::Entry::new();
        label_entry.set_hexpand(true);
        let button = gtk::Button::new();
        input_row.append(&input_label);
        input_row.append(&entry);
        input_row.append(&label_entry);
        input_row.append(&button);
        let toggles = gtk::Box::new(gtk::Orientation::Vertical, 0);
        toggles.add_css_class("toggles");
        let buttons = gtk::Box::new(gtk::Orientation::Vertical, 0);
        buttons.add_css_class("buttons");
        let hints = gtk::Grid::new();
        hints.add_css_class("hints");
        hints.set_column_homogeneous(true);
        frame.append(&title);
        frame.append(&info);
        frame.append(&calendar.root);
        frame.append(&buttons);
        frame.append(&toggles);
        frame.append(&input_row);
        frame.append(&hints);
        fixed.put(&frame, 0.0, 0.0);
        popup.set_child(Some(&fixed));
        Rc::new(Card {
            popup,
            fixed,
            frame,
            title,
            info,
            calendar,
            hints,
            input_row,
            input_label,
            entry,
            label_entry,
            button,
            input_command: RefCell::new(None),
            toggles,
            toggle_rows: RefCell::new(Vec::new()),
            quiet: Cell::new(false),
            buttons,
            button_rows: RefCell::new(Vec::new()),
            scripts: scripts.to_string(),
        })
    }

    /// The input's command, with what was typed in place of `{value}` and
    /// `{label}`.
    fn input_command(&self) -> Option<String> {
        let value = self.entry.text().to_string();
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        let label = self.label_entry.text().to_string();
        self.input_command
            .borrow()
            .as_ref()
            .map(|c| c.replace("{value}", value).replace("{label}", label.trim()).trim_end().to_string())
    }

    /// `toggles`: each switch's line and state, rendered; `states`: each
    /// button row's state.
    fn show(self: &Rc<Self>, module: &Module, info: &str, toggles: &[(String, bool)], states: &[String], x: i32, bar_width: i32) {
        self.title.set_text(&module.title);
        self.set_info(info, module.markup);
        self.show_toggles(module, toggles);
        self.show_buttons(module, states);
        // the calendar opens on the current month, like its own window did
        let with_calendar = module.kind == "calendar";
        self.calendar.root.set_visible(with_calendar);
        if with_calendar {
            self.calendar.open();
        }
        // the keyboard goes with an input: sway hands it back to the
        // previous window when the mode drops or the popup unmaps
        self.popup.set_keyboard_mode(if module.input.is_some() {
            KeyboardMode::Exclusive
        } else {
            KeyboardMode::OnDemand
        });
        match &module.input {
            Some(input) => {
                self.input_label.set_text(&input.prompt);
                self.input_label.set_visible(!input.prompt.is_empty());
                self.entry.set_text(&input.default);
                self.label_entry.set_text(&input.label_default);
                self.label_entry
                    .set_placeholder_text(Some(input.label.as_str()).filter(|l| !l.is_empty()));
                self.label_entry.set_visible(!input.label.is_empty());
                self.button.set_label(&input.button);
                *self.input_command.borrow_mut() = Some(input.command.clone());
                self.input_row.set_visible(true);
            }
            None => {
                *self.input_command.borrow_mut() = None;
                self.input_row.set_visible(false);
            }
        }
        while let Some(c) = self.hints.first_child() {
            self.hints.remove(&c);
        }
        let hints: Vec<&[String; 2]> = module.hints.iter().filter(|h| !h[0].is_empty()).collect();
        for (i, h) in hints.iter().enumerate() {
            let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            cell.add_css_class("hint");
            cell.set_halign(gtk::Align::Start);
            let key = gtk::Label::new(Some(&h[0]));
            key.add_css_class("key");
            cell.append(&key);
            cell.append(&gtk::Label::new(Some(&h[1])));
            self.hints.attach(&cell, (i % 2) as i32, (i / 2) as i32, 1, 1);
        }
        self.hints.set_visible(!hints.is_empty());
        self.frame.set_visible(true);
        // the calendar pins to the monitor's right edge, with the same
        // gap it keeps under the bar, by the card's real width (its
        // content can grow past card_width); the frame is measured
        // visible, a hidden widget measures zero
        let x = if module.kind == "calendar" && module.side == "right" {
            let (_, width, _, _) = self.frame.measure(gtk::Orientation::Horizontal, -1);
            bar_width - width - 6
        } else {
            x
        };
        self.fixed.move_(&self.frame, x as f64, 0.0);
        if !self.popup.is_visible() {
            self.popup.present();
        }
        if module.input.is_some() {
            self.entry.grab_focus();
            self.entry.select_region(0, -1);
        }
        // the input region follows the card, once it is laid out
        let me = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(30), move || me.update_input_region());
    }

    /// The popup window goes with the card: GTK 4 does not repaint a
    /// layer surface whose only child was hidden, the old card would stay
    /// on screen.
    fn hide(&self) {
        self.frame.set_visible(false);
        self.popup.set_visible(false);
    }

    /// The switches' rows, built anew for the module shown. A switch the
    /// user moves runs its command; the state comes back through the
    /// variables, as from any other change (update_toggles).
    fn show_toggles(self: &Rc<Self>, module: &Module, values: &[(String, bool)]) {
        while let Some(c) = self.toggles.first_child() {
            self.toggles.remove(&c);
        }
        let mut rows = Vec::new();
        for t in &module.toggles {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            row.add_css_class("toggle");
            let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
            text.set_hexpand(true);
            text.set_valign(gtk::Align::Center);
            let name = gtk::Label::new(Some(&t.label));
            name.add_css_class("name");
            name.set_xalign(0.0);
            let line = gtk::Label::new(None);
            line.add_css_class("detail");
            line.set_xalign(0.0);
            line.set_ellipsize(gtk::pango::EllipsizeMode::End);
            text.append(&name);
            text.append(&line);
            let switch = gtk::Switch::new();
            switch.set_valign(gtk::Align::Center);
            // the card holds the rows, a row holds the card weakly
            let weak: Weak<Card> = Rc::downgrade(self);
            let (on, off) = (t.on.clone(), t.off.clone());
            switch.connect_state_set(move |_, state| {
                if let Some(c) = weak.upgrade() {
                    if !c.quiet.get() {
                        runner::detached(if state { &on } else { &off }, &c.scripts);
                    }
                }
                glib::Propagation::Proceed
            });
            row.append(&text);
            row.append(&switch);
            self.toggles.append(&row);
            rows.push((line, switch));
        }
        self.toggles.set_visible(!rows.is_empty());
        *self.toggle_rows.borrow_mut() = rows;
        self.update_toggles(values);
    }

    /// The switches show the states as they are now.
    fn update_toggles(&self, values: &[(String, bool)]) {
        for ((line, switch), (detail, on)) in self.toggle_rows.borrow().iter().zip(values) {
            line.set_text(detail);
            line.set_visible(!detail.is_empty());
            if switch.is_active() != *on {
                self.quiet.set(true);
                switch.set_active(*on);
                self.quiet.set(false);
            }
        }
    }

    /// The rows of buttons, built anew for the module shown; a click
    /// runs the button's command.
    fn show_buttons(&self, module: &Module, states: &[String]) {
        while let Some(c) = self.buttons.first_child() {
            self.buttons.remove(&c);
        }
        let mut rows = Vec::new();
        for r in &module.buttons {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
            row.add_css_class("button-row");
            if !r.label.is_empty() {
                let caption = gtk::Label::new(Some(&r.label));
                caption.add_css_class("name");
                caption.set_xalign(0.0);
                row.append(&caption);
            }
            let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            line.set_homogeneous(true);
            let mut buttons = Vec::new();
            for b in &r.items {
                let button = gtk::Button::with_label(&b.text);
                let (cmd, scripts) = (b.command.clone(), self.scripts.clone());
                button.connect_clicked(move |_| runner::detached(&cmd, &scripts));
                line.append(&button);
                buttons.push((button, b.value.clone()));
            }
            row.append(&line);
            self.buttons.append(&row);
            rows.push(buttons);
        }
        self.buttons.set_visible(!rows.is_empty());
        *self.button_rows.borrow_mut() = rows;
        self.update_buttons(states);
    }

    /// The button whose value is its row's state is the active one.
    fn update_buttons(&self, states: &[String]) {
        for (row, state) in self.button_rows.borrow().iter().zip(states) {
            for (button, value) in row {
                if !value.is_empty() && value == state {
                    button.add_css_class("active");
                } else {
                    button.remove_css_class("active");
                }
            }
        }
    }

    fn set_info(&self, text: &str, markup: bool) {
        self.info.set_visible(!text.is_empty());
        if markup {
            self.info.set_markup(text);
        } else {
            self.info.set_text(text);
        }
    }

    /// The popup takes the pointer only over the card.
    fn update_input_region(&self) {
        let Some(surface) = self.popup.surface() else { return };
        let bounds = self.frame.is_visible().then(|| self.frame.compute_bounds(&self.popup)).flatten();
        let region = match bounds {
            Some(b) => cairo::Region::create_rectangle(&cairo::RectangleInt::new(
                b.x() as i32,
                b.y() as i32,
                b.width().ceil() as i32,
                b.height().ceil() as i32,
            )),
            None => cairo::Region::create(),
        };
        surface.set_input_region(Some(&region));
    }
}

/// Hover state shared by the modules of one output.
struct Hover {
    /// The open module's index.
    open: Cell<Option<usize>>,
    /// Its widget, to take the mark off when the card closes from itself.
    open_widget: RefCell<Option<gtk::Box>>,
    /// Stamped on every enter; a leave closes only if nothing was entered
    /// since.
    seq: Cell<u64>,
}

/// A leave happened `delay` ago and nothing was entered since: close.
fn schedule_close(hover: &Rc<Hover>, card: &Rc<Card>, delay: Duration) {
    let seq = hover.seq.get();
    let (hover, card) = (hover.clone(), card.clone());
    glib::timeout_add_local_once(delay, move || {
        if hover.seq.get() != seq {
            return;
        }
        close_card(&hover, &card);
    });
}

fn close_card(hover: &Hover, card: &Card) {
    hover.open.set(None);
    if let Some(w) = hover.open_widget.borrow_mut().take() {
        w.remove_css_class("open");
    }
    card.hide();
}

struct ModuleUi {
    cfg: Module,
    root: gtk::Box,
    label: gtk::Label,
    text: Template,
    info: Template,
    class: Template,
    /// The switches' line and state templates.
    toggles: Vec<(Template, Template)>,
    /// The button rows' state templates.
    button_states: Vec<Template>,
    prev_class: RefCell<Vec<String>>,
}

impl ModuleUi {
    fn toggle_values(&self, lookup: &dyn Fn(&str) -> Option<serde_json::Value>) -> Vec<(String, bool)> {
        self.toggles
            .iter()
            .map(|(detail, state)| (detail.render(lookup), truthy(&state.render(lookup))))
            .collect()
    }

    fn button_states(&self, lookup: &dyn Fn(&str) -> Option<serde_json::Value>) -> Vec<String> {
        self.button_states.iter().map(|t| t.render(lookup)).collect()
    }
}

fn layer_window(title: &str, class: &str, namespace: &str, layer: Layer, monitor: &gdk::Monitor) -> gtk::Window {
    let w = gtk::Window::new();
    w.set_title(Some(title));
    w.add_css_class(class);
    w.set_decorated(false);
    w.init_layer_shell();
    w.set_namespace(Some(namespace));
    w.set_layer(layer);
    w.set_monitor(Some(monitor));
    for edge in [Edge::Top, Edge::Left, Edge::Right] {
        w.set_anchor(edge, true);
    }
    w
}

pub fn build(
    cfg: &Arc<Config>,
    store: &Rc<Store>,
    tray: &Rc<Tray>,
    monitor: &gdk::Monitor,
    id: u32,
    reqs: UnboundedSender<Request>,
) -> OutputUi {
    let bar = layer_window("lintel", "lintel-bar", "lintel-bar", Layer::Top, monitor);
    bar.auto_exclusive_zone_enable();
    bar.set_keyboard_mode(KeyboardMode::None);

    let popup = layer_window("lintel popup", "lintel-popup", "lintel-popup", Layer::Overlay, monitor);
    popup.set_margin(Edge::Top, cfg.bar.height);
    // -1: placed from the screen's edge, not below the bar's exclusive zone
    popup.set_exclusive_zone(-1);
    // the keyboard on demand; a card with an input takes it (Card::show)
    popup.set_keyboard_mode(KeyboardMode::OnDemand);
    let card = Card::new(popup.clone(), cfg.bar.card_width, &cfg.bar.scripts);

    let line = gtk::CenterBox::new();
    line.add_css_class("line");
    line.set_size_request(-1, cfg.bar.height);
    let left = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    left.add_css_class("left");
    let right = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    right.add_css_class("right");
    line.set_start_widget(Some(&left));
    line.set_end_widget(Some(&right));
    bar.set_child(Some(&line));

    let hover = Rc::new(Hover {
        open: Cell::new(None),
        open_widget: RefCell::new(None),
        seq: Cell::new(0),
    });
    let scripts = cfg.bar.scripts.clone();
    let delay = Duration::from_millis(cfg.bar.leave_delay_ms);

    // into the card: stays open; out of it: closes like leaving a module
    {
        let motion = gtk::EventControllerMotion::new();
        let h = hover.clone();
        motion.connect_enter(move |_, _, _| h.seq.set(h.seq.get() + 1));
        let (h, c) = (hover.clone(), card.clone());
        motion.connect_leave(move |_| schedule_close(&h, &c, delay));
        card.frame.add_controller(motion);
    }
    // the pointer leaving either window altogether (a module's own leave
    // does not always come when the pointer jumps off the surface)
    for w in [&bar, &popup] {
        let motion = gtk::EventControllerMotion::new();
        let (h, c) = (hover.clone(), card.clone());
        motion.connect_leave(move |_| schedule_close(&h, &c, delay));
        w.add_controller(motion);
    }
    // the calendar closes the card from the keyboard (q, Escape)
    {
        let h = hover.clone();
        let weak: Weak<Card> = Rc::downgrade(&card);
        card.calendar.set_on_close(move || {
            h.seq.set(h.seq.get() + 1);
            if let Some(c) = weak.upgrade() {
                close_card(&h, &c);
            }
        });
    }
    // the input: the button, or Enter in either entry, runs the command
    // and closes the card, the default value too
    {
        let run = {
            let (h, c, scripts) = (hover.clone(), card.clone(), scripts.clone());
            Rc::new(move || {
                if let Some(cmd) = c.input_command() {
                    runner::detached(&cmd, &scripts);
                }
                h.seq.set(h.seq.get() + 1);
                close_card(&h, &c);
            })
        };
        // Escape closes it without running anything
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (h, c) = (hover.clone(), card.clone());
        keys.connect_key_pressed(move |_, key, _, _| {
            if key != gdk::Key::Escape {
                return glib::Propagation::Proceed;
            }
            h.seq.set(h.seq.get() + 1);
            close_card(&h, &c);
            glib::Propagation::Stop
        });
        card.input_row.add_controller(keys);
        let r = run.clone();
        card.button.connect_clicked(move |_| r());
        let r = run.clone();
        card.entry.connect_activate(move |_| r());
        card.label_entry.connect_activate(move |_| run());
    }

    for (index, m) in cfg.modules.iter().enumerate() {
        let parent = if m.side == "left" { &left } else { &right };
        match m.kind.as_str() {
            "list" | "workspaces" => build_list(m, parent, store, id, &scripts),
            "tray" => {
                let b = gtk::Box::new(gtk::Orientation::Horizontal, m.spacing);
                b.add_css_class("tray");
                parent.append(&b);
                tray.add_box(id, b, m.icon_size);
            }
            _ => build_module(index, m, parent, &line, store, id, &scripts, &hover, &card, &reqs, cfg),
        }
    }

    bar.present();
    card.hide();
    OutputUi {
        monitor: monitor.clone(),
        id,
        bar,
        popup,
    }
}

/// One button per item of an array: the workspaces, the countdowns. The
/// box carries the module's name as class, each button `item` plus the
/// item's class template.
fn build_list(m: &Module, parent: &gtk::Box, store: &Rc<Store>, owner: u32, scripts: &str) {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    b.add_css_class("list");
    b.add_css_class(&m.name);
    parent.append(&b);
    // `sway.workspaces`: the variable is `sway`, the array is inside it
    let (var, path) = match m.items.split_once('.') {
        Some((v, p)) => (v.to_string(), p.to_string()),
        None => (m.items.clone(), String::new()),
    };
    let subscribed = var.clone();
    let text_t = Template::parse(&m.item_text);
    let class_t = Template::parse(&m.item_class);
    let click_t = Template::parse(&m.item_click);
    let middle_t = Template::parse(&m.item_middle);
    let right_t = Template::parse(&m.item_right);
    let scripts = scripts.to_string();
    let bb = b.clone();
    // what was drawn last: the variable changes more often than the list
    // (every window title, for the workspaces), the buttons must not
    let drawn: RefCell<Vec<(String, String)>> = RefCell::new(Vec::new());
    let render: super::store::Callback = Rc::new(move |store: &Store| {
        let Some(root) = store.get(&var) else { return };
        let Some(serde_json::Value::Array(list)) = crate::template::walk_path(&root, &path) else {
            return;
        };
        let wanted: Vec<(String, String)> = list
            .iter()
            .map(|item| (text_t.render_item(item), class_t.render_item(item)))
            .collect();
        if *drawn.borrow() == wanted {
            return;
        }
        *drawn.borrow_mut() = wanted.clone();
        while let Some(c) = bb.first_child() {
            bb.remove(&c);
        }
        for (item, (text, classes)) in list.iter().zip(wanted.iter()) {
            let button = gtk::Button::with_label(text);
            button.add_css_class("item");
            for c in classes.split_whitespace() {
                button.add_css_class(c);
            }
            let cmds = [click_t.render_item(item), middle_t.render_item(item), right_t.render_item(item)];
            let scripts = scripts.clone();
            let gesture = gtk::GestureClick::new();
            gesture.set_button(0);
            gesture.connect_pressed(move |g, _, _, _| {
                g.set_state(gtk::EventSequenceState::Claimed);
                if let Some(cmd) = cmds.get(g.current_button() as usize - 1) {
                    runner::detached(cmd, &scripts);
                }
            });
            button.add_controller(gesture);
            bb.append(&button);
        }
    });
    store.subscribe([subscribed], owner, render.clone());
    render(store);
}

#[allow(clippy::too_many_arguments)]
fn build_module(
    index: usize,
    m: &Module,
    parent: &gtk::Box,
    line: &gtk::CenterBox,
    store: &Rc<Store>,
    owner: u32,
    scripts: &str,
    hover: &Rc<Hover>,
    card: &Rc<Card>,
    reqs: &UnboundedSender<Request>,
    cfg: &Arc<Config>,
) {
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    root.add_css_class("module");
    root.add_css_class(&m.name);
    let slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    slot.add_css_class("slot");
    let label = gtk::Label::new(None);
    // a label asks for its whole text: the sum over the modules can pass
    // the output's width, and GTK never allocates the window below its
    // minimum, so the bar would run off the screen. With a cap the label
    // is the one that shrinks, with an ellipsis, and the rest stays whole.
    if let Some(n) = m.max_chars {
        label.set_max_width_chars(n);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    }
    slot.append(&label);
    root.append(&slot);
    parent.append(&root);

    let ui = Rc::new(ModuleUi {
        cfg: m.clone(),
        root: root.clone(),
        label,
        text: Template::parse(&m.text),
        info: Template::parse(&m.info),
        class: Template::parse(&m.class),
        toggles: m
            .toggles
            .iter()
            .map(|t| (Template::parse(&t.detail), Template::parse(&t.state)))
            .collect(),
        button_states: m.buttons.iter().map(|r| Template::parse(&r.state)).collect(),
        prev_class: RefCell::new(Vec::new()),
    });

    // redraw on a change of any variable the templates read
    let mut vars: Vec<String> = ui
        .text
        .vars()
        .iter()
        .chain(ui.info.vars())
        .chain(ui.class.vars())
        .chain(ui.toggles.iter().flat_map(|(d, s)| d.vars().iter().chain(s.vars())))
        .chain(ui.button_states.iter().flat_map(|t| t.vars()))
        .cloned()
        .collect();
    vars.sort();
    vars.dedup();
    let render: super::store::Callback = {
        let ui = ui.clone();
        let hover = hover.clone();
        let card = card.clone();
        Rc::new(move |store: &Store| {
            let lookup = |name: &str| store.get(name);
            let text = ui.text.render(&lookup);
            if ui.cfg.markup {
                ui.label.set_markup(&text);
            } else {
                ui.label.set_text(&text);
            }
            let hidden = text.is_empty() || ui.cfg.hide_when.as_deref() == Some(text.as_str());
            ui.root.set_visible(!hidden);
            for c in ui.prev_class.borrow().iter() {
                ui.root.remove_css_class(c);
            }
            let classes: Vec<String> = ui.class.render(&lookup).split_whitespace().map(String::from).collect();
            for c in &classes {
                ui.root.add_css_class(c);
            }
            *ui.prev_class.borrow_mut() = classes;
            if hover.open.get() == Some(index) {
                card.set_info(&ui.info.render(&lookup), ui.cfg.markup);
                card.update_toggles(&ui.toggle_values(&lookup));
                card.update_buttons(&ui.button_states(&lookup));
            }
        })
    };
    store.subscribe(vars.clone(), owner, render.clone());
    render(store);

    // the mouse
    let s = scripts.to_string();
    let (click, middle, right) = (m.click.clone(), m.middle.clone(), m.right.clone());
    let gesture = gtk::GestureClick::new();
    gesture.set_button(0);
    gesture.connect_pressed(move |g, _, _, _| match g.current_button() {
        1 => runner::detached(&click, &s),
        2 => runner::detached(&middle, &s),
        3 => runner::detached(&right, &s),
        _ => {}
    });
    root.add_controller(gesture);
    let s = scripts.to_string();
    let (up, down) = (m.scroll_up.clone(), m.scroll_down.clone());
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.connect_scroll(move |_, _, dy| {
        if dy < 0.0 {
            runner::detached(&up, &s);
        } else if dy > 0.0 {
            runner::detached(&down, &s);
        }
        glib::Propagation::Stop
    });
    root.add_controller(scroll);

    if !matches!(m.kind.as_str(), "module" | "calendar") {
        return;
    }
    // the card: opens when the pointer rests on the module for a beat, so
    // a flick over the bar does not pop cards by; closes a moment after a
    // leave unless something else was entered meanwhile
    let weak_store: Weak<Store> = Rc::downgrade(store);
    let (hover_in, card_in, ui_in, line_in, reqs_in) = (hover.clone(), card.clone(), ui.clone(), line.clone(), reqs.clone());
    let card_width = cfg.bar.card_width;
    let open_delay = Duration::from_millis(cfg.bar.hover_delay_ms);
    let opening: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let motion = gtk::EventControllerMotion::new();
    let opening_in = opening.clone();
    motion.connect_enter(move |c, _, _| {
        let Some(w) = c.widget() else { return };
        if opening_in.borrow().is_some() {
            return;
        }
        // an enter happened: a close the module before scheduled is
        // out of date
        hover_in.seq.set(hover_in.seq.get() + 1);
        let (hover_d, card_d, ui_d, line_d, reqs_d) = (hover_in.clone(), card_in.clone(), ui_in.clone(), line_in.clone(), reqs_in.clone());
        let (weak_d, vars_d, opening_d) = (weak_store.clone(), vars.clone(), opening_in.clone());
        opening_in.replace(Some(glib::timeout_add_local_once(open_delay, move || {
            opening_d.replace(None);
            if hover_d.open.get() != Some(index) {
                // the previous module loses its mark
                hover_d.open.set(None);
                if let Some(prev) = hover_d.open_widget.borrow_mut().take() {
                    prev.remove_css_class("open");
                }
            }
            let Some(store) = weak_d.upgrade() else { return };
            let lookup = |name: &str| store.get(name);
            let info = ui_d.info.render(&lookup);
            let mx = w.compute_bounds(&line_d).map(|b| b.x() as i32).unwrap_or(0);
            let mw = w.width();
            let bar_width = line_d.width();
            let x = if ui_d.cfg.side == "left" { mx } else { mx + mw - card_width };
            let x = x.clamp(0, (bar_width - card_width).max(0));
            hover_d.open.set(Some(index));
            *hover_d.open_widget.borrow_mut() = Some(ui_d.root.clone());
            w.add_css_class("open");
            card_d.show(
                &ui_d.cfg,
                &info,
                &ui_d.toggle_values(&lookup),
                &ui_d.button_states(&lookup),
                x,
                bar_width,
            );
            // sources that refresh on hover take their snapshot now
            for var in &vars_d {
                let _ = reqs_d.send(Request::Hover(var.clone()));
            }
        })));
    });
    let (hover_out, card_out, opening_out) = (hover.clone(), card.clone(), opening.clone());
    let close_delay = Duration::from_millis(cfg.bar.leave_delay_ms);
    motion.connect_leave(move |_| {
        // a rest that never became an open is simply cancelled
        if let Some(id) = opening_out.borrow_mut().take() {
            id.remove();
        }
        schedule_close(&hover_out, &card_out, close_delay);
    });
    root.add_controller(motion);
}
