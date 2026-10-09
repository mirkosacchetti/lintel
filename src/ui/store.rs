//! The variables on the GTK side, and who redraws when one changes.
//! Setting a variable calls the subscribers of that variable, nothing
//! else: a value nobody shows costs nothing, a value shown in three places
//! redraws three widgets.

use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub type Callback = Rc<dyn Fn(&Store)>;

struct Sub {
    var: String,
    owner: u32,
    f: Callback,
}

#[derive(Default)]
pub struct Store {
    values: RefCell<HashMap<String, Value>>,
    subs: RefCell<Vec<Sub>>,
}

impl Store {
    pub fn get(&self, name: &str) -> Option<Value> {
        self.values.borrow().get(name).cloned()
    }

    pub fn set(&self, name: &str, value: Value) {
        let changed = self.values.borrow().get(name) != Some(&value);
        if !changed {
            return;
        }
        self.values.borrow_mut().insert(name.to_string(), value);
        // the callbacks may subscribe or read: no borrow held while they run
        let fs: Vec<Callback> = self.subs.borrow().iter().filter(|s| s.var == name).map(|s| s.f.clone()).collect();
        for f in fs {
            f(self);
        }
    }

    /// `f` runs whenever one of `vars` changes; `owner` tags the
    /// subscription so a window going away can drop its callbacks.
    pub fn subscribe<I: IntoIterator<Item = String>>(&self, vars: I, owner: u32, f: Callback) {
        let mut subs = self.subs.borrow_mut();
        for var in vars {
            subs.push(Sub { var, owner, f: f.clone() });
        }
    }

    pub fn drop_owner(&self, owner: u32) {
        self.subs.borrow_mut().retain(|s| s.owner != owner);
    }
}
