//! Templates: text with `{var}`, `{var.key.0}` and `{var.key|trunc:60}`
//! placeholders, filled from the variable store.

use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone)]
enum Piece {
    Text(String),
    Hole { path: Vec<String>, filters: Vec<String> },
}

#[derive(Debug, Clone, Default)]
pub struct Template {
    pieces: Vec<Piece>,
    vars: HashSet<String>,
}

impl Template {
    pub fn parse(text: &str) -> Template {
        let mut pieces = Vec::new();
        let mut vars = HashSet::new();
        let mut buf = String::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let (before, after) = rest.split_at(start);
            if let Some(end) = after.find('}') {
                let inner = &after[1..end];
                // `{}` or anything with spaces is not a placeholder
                if inner.is_empty() || inner.contains(char::is_whitespace) {
                    buf.push_str(before);
                    buf.push_str(&after[..end + 1]);
                    rest = &after[end + 1..];
                    continue;
                }
                buf.push_str(before);
                if !buf.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut buf)));
                }
                let mut parts = inner.split('|');
                let path: Vec<String> = parts.next().unwrap().split('.').map(String::from).collect();
                let filters = parts.map(String::from).collect();
                vars.insert(path[0].clone());
                pieces.push(Piece::Hole { path, filters });
                rest = &after[end + 1..];
            } else {
                buf.push_str(rest);
                rest = "";
            }
        }
        buf.push_str(rest);
        if !buf.is_empty() {
            pieces.push(Piece::Text(buf));
        }
        Template { pieces, vars }
    }

    /// The variables this template reads (the first path segment).
    pub fn vars(&self) -> &HashSet<String> {
        &self.vars
    }

    /// Fill the template; `lookup` gives a variable's current value.
    pub fn render(&self, lookup: &dyn Fn(&str) -> Option<Value>) -> String {
        let mut out = String::new();
        for p in &self.pieces {
            match p {
                Piece::Text(t) => out.push_str(t),
                Piece::Hole { path, filters } => {
                    let root = lookup(&path[0]);
                    let v = root.as_ref().and_then(|r| walk(r, &path[1..]));
                    let mut s = v.map(stringify).unwrap_or_default();
                    for f in filters {
                        s = apply(f, s);
                    }
                    out.push_str(&s);
                }
            }
        }
        out
    }

    /// Fill the template from one value: `{name}` is `value.name`.
    pub fn render_item(&self, item: &Value) -> String {
        let mut out = String::new();
        for p in &self.pieces {
            match p {
                Piece::Text(t) => out.push_str(t),
                Piece::Hole { path, filters } => {
                    let mut s = walk(item, path).map(stringify).unwrap_or_default();
                    for f in filters {
                        s = apply(f, s);
                    }
                    out.push_str(&s);
                }
            }
        }
        out
    }
}

/// `value.key.0` on a value: the dotted path after the variable name.
pub fn walk_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(v);
    }
    let parts: Vec<String> = path.split('.').map(String::from).collect();
    walk(v, &parts)
}

fn walk<'a>(v: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut cur = v;
    for key in path {
        cur = match cur {
            Value::Object(m) => m.get(key)?,
            Value::Array(a) => a.get(key.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn apply(filter: &str, s: String) -> String {
    let (name, arg) = filter.split_once(':').unwrap_or((filter, ""));
    match name {
        // cut to N characters, with an ellipsis
        "trunc" => {
            let n: usize = arg.parse().unwrap_or(usize::MAX);
            if s.chars().count() > n {
                let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
                t.push('…');
                t
            } else {
                s
            }
        }
        "upper" => s.to_uppercase(),
        "lower" => s.to_lowercase(),
        // a value when the string is empty
        "or" => {
            if s.is_empty() {
                arg.to_string()
            } else {
                s
            }
        }
        // the argument when the value is true-ish (true, non-zero, non-empty)
        "if" => {
            if matches!(s.as_str(), "" | "false" | "0" | "null") {
                String::new()
            } else {
                arg.to_string()
            }
        }
        // escape for pango markup
        "esc" => s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;"),
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_paths_and_filters() {
        let t = Template::parse("| {sway.title|trunc:5} [{sway.ws.1.name}] {} {x|or:none}");
        let v = json!({"title": "hello world", "ws": [{"name": "a"}, {"name": "b"}]});
        let out = t.render(&|name| if name == "sway" { Some(v.clone()) } else { None });
        assert_eq!(out, "| hell… [b] {} none");
        let c = Template::parse("{focused|if:focused} {urgent|if:urgent}").render_item(&json!({"focused": true, "urgent": false}));
        assert_eq!(c, "focused ");
        assert!(t.vars().contains("sway") && t.vars().contains("x"));
    }
}
