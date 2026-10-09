//! The calendar in a `calendar` module's card. A month grid, Monday
//! first, the days of the months before and after dimmed, today lit; the
//! names of months and days are the system locale's (LC_TIME). ‹ and ›,
//! or a scroll, move a month; the month name returns to today. With the
//! card's keyboard: ← and → (j and ;) the
//! months, ↑ and ↓ (k and l) the years, `t` today, `q` or Escape closes
//! the card.

use crate::locale;
use chrono::{Datelike, Days, Local, Locale, NaiveDate};
use gtk::gdk::Key;
use gtk::glib;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// 2024-01-01 is a Monday: the weekdays' names, Monday first.
const WEEK_ONE: (i32, u32, u32) = (2024, 1, 1);

pub struct Calendar {
    pub root: gtk::Box,
    title: gtk::Button,
    days: Vec<gtk::Label>,
    /// The month on show: (year, month 1–12).
    shown: Cell<(i32, u32)>,
    locale: Locale,
    on_close: RefCell<Option<Rc<dyn Fn()>>>,
}

impl Calendar {
    pub fn new() -> Rc<Calendar> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("calendar");
        root.set_visible(false);

        let head = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        head.add_css_class("head");
        let prev = gtk::Button::with_label("‹");
        prev.add_css_class("nav");
        let title = gtk::Button::with_label(" ");
        title.add_css_class("title");
        title.set_hexpand(true);
        let next = gtk::Button::with_label("›");
        next.add_css_class("nav");
        head.append(&prev);
        head.append(&title);
        head.append(&next);

        let grid = gtk::Grid::new();
        grid.set_column_homogeneous(true);
        grid.set_row_homogeneous(true);
        let locale = locale::time();
        // the week row: the locale's short weekday names, from Monday
        let (wy, wm, wd) = WEEK_ONE;
        for i in 0..7u64 {
            let name = NaiveDate::from_ymd_opt(wy, wm, wd)
                .and_then(|monday| monday.checked_add_days(Days::new(i)))
                .map(|day| day.format_localized("%a", locale).to_string())
                .unwrap_or_default();
            let l = gtk::Label::new(Some(&name));
            l.add_css_class("weekday");
            if i >= 5 {
                l.add_css_class("weekend");
            }
            grid.attach(&l, i as i32, 0, 1, 1);
        }
        let mut days = Vec::with_capacity(42);
        for i in 0..42 {
            let l = gtk::Label::new(None);
            l.add_css_class("day");
            grid.attach(&l, i % 7, i / 7 + 1, 1, 1);
            days.push(l);
        }
        root.append(&head);
        root.append(&grid);

        let now = Local::now();
        let cal = Rc::new(Calendar {
            root: root.clone(),
            title,
            days,
            shown: Cell::new((now.year(), now.month())),
            locale,
            on_close: RefCell::new(None),
        });
        cal.rebuild();

        {
            let c = cal.clone();
            prev.connect_clicked(move |_| c.move_months(-1));
            let c = cal.clone();
            next.connect_clicked(move |_| c.move_months(1));
            let c = cal.clone();
            cal.title.connect_clicked(move |_| c.open());
        }
        // a scroll over the grid moves a month, like the scroll on a module
        {
            let c = cal.clone();
            let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
            scroll.connect_scroll(move |_, _, dy| {
                if dy > 0.0 {
                    c.move_months(1);
                } else if dy < 0.0 {
                    c.move_months(-1);
                }
                glib::Propagation::Stop
            });
            root.add_controller(scroll);
        }
        {
            let c = cal.clone();
            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, _| {
                match key {
                    Key::Left | Key::j => c.move_months(-1),
                    Key::Right | Key::semicolon => c.move_months(1),
                    Key::Up | Key::k => c.move_years(-1),
                    Key::Down | Key::l => c.move_years(1),
                    Key::t => c.open(),
                    Key::Escape | Key::q => {
                        if let Some(f) = c.on_close.borrow().as_ref() {
                            f();
                        }
                    }
                    _ => return glib::Propagation::Proceed,
                }
                glib::Propagation::Stop
            });
            root.add_controller(keys);
        }
        cal
    }

    /// Closing the card from the keyboard (q, Escape): set by the bar.
    pub fn set_on_close(&self, f: impl Fn() + 'static) {
        *self.on_close.borrow_mut() = Some(Rc::new(f));
    }

    /// Back on the current month: what the card opens on.
    pub fn open(&self) {
        let now = Local::now();
        self.show_month(now.year(), now.month());
    }

    fn move_months(&self, delta: i32) {
        let (year, month) = self.shown.get();
        let index = year as i64 * 12 + (month as i64 - 1) + delta as i64;
        self.show_month(index.div_euclid(12) as i32, index.rem_euclid(12) as u32 + 1);
    }

    fn move_years(&self, delta: i32) {
        let (year, month) = self.shown.get();
        self.show_month(year + delta, month);
    }

    fn show_month(&self, year: i32, month: u32) {
        self.shown.set((year, month));
        self.rebuild();
    }

    fn rebuild(&self) {
        let (year, month) = self.shown.get();
        let Some(first) = NaiveDate::from_ymd_opt(year, month, 1) else {
            return;
        };
        self.title.set_label(&first.format_localized("%B %Y", self.locale).to_string());
        let offset = first.weekday().num_days_from_monday() as i32;
        let days = days_in_month(year, month);
        let (py, pm) = if month == 1 { (year - 1, 12) } else { (year, month - 1) };
        let prev_days = days_in_month(py, pm);
        let today = Local::now();
        let is_today_month = today.year() == year && today.month() == month;

        for (i, l) in self.days.iter().enumerate() {
            let day = i as i32 - offset + 1;
            l.remove_css_class("outside");
            l.remove_css_class("today");
            if day < 1 {
                l.set_label(&(prev_days + day).to_string());
                l.add_css_class("outside");
            } else if day > days {
                l.set_label(&(day - days).to_string());
                l.add_css_class("outside");
            } else {
                l.set_label(&day.to_string());
                if is_today_month && today.day() as i32 == day {
                    l.add_css_class("today");
                }
            }
        }
    }
}

fn days_in_month(year: i32, month: u32) -> i32 {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let Some(next_first) = NaiveDate::from_ymd_opt(ny, nm, 1) else {
        return 30;
    };
    (next_first - Days::new(1)).day() as i32
}
