// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: LicenseRef-Zyvor-Production-2.0

//! The Go-template subset behind `fluxctl mac inspect --format` and `mac images --format`.
//!
//! Supported, and nothing else (anything else is refused with this list in the message):
//!
//! - `{{.State.Status}}`: a field path into the JSON document. `{{.}}` is the current value.
//! - `{{json X}}` (or `{{X | json}}`): compact JSON of a value.
//! - `{{index X KEY...}}`: a map key (`{{index .Labels "team"}}`) or an array position (`{{index .Mounts 0}}`).
//! - `{{range X}}...{{end}}`: repeat for every element of an array (or every value of an object, in key order); inside, `.`
//!   is the element.
//! - `"text"` and whole numbers as arguments, and `\n` / `\t` in the text between actions.
//!
//! Field names match without regard to case or underscores, so `.State.Status`, `.state.status` and `.Resources.MemoryMiB`
//! (the document's `memory_mib`) all work. A value that is missing or null prints `<no value>`; a string prints as is; a
//! number or boolean prints as JSON; an object or array prints as compact JSON (Go would print `map[...]`).
//!
//! The format text is parsed before anything else happens, so a bad template fails before the daemon is asked.

use anyhow::{Result, bail};
use serde_json::Value;

/// What the error message says is supported; also shown in `mac inspect --help`.
pub const SUPPORTED: &str =
    "{{.Field.Path}}, {{json X}}, {{index X KEY}} and {{range X}}...{{end}}";

#[derive(Debug, Clone, PartialEq)]
enum Operand {
    /// `.` or `.a.b`.
    Path(Vec<String>),
    Str(String),
    Int(i64),
}

#[derive(Debug, Clone, PartialEq)]
enum Func {
    Json,
    Index,
}

/// One stage of a pipeline: an operand alone, or a function with arguments.
#[derive(Debug, Clone, PartialEq)]
enum Stage {
    Value(Operand),
    Call(Func, Vec<Operand>),
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Action(String, Vec<Stage>),
    Range(Operand, Vec<Node>),
}

/// A parsed format string.
#[derive(Debug, Clone)]
pub struct Template {
    nodes: Vec<Node>,
}

fn unsupported(what: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "unsupported template syntax {what}: --format understands only {SUPPORTED} (see `fluxctl explain MAC_TEMPLATE_SYNTAX`)"
    )
}

fn unescape(s: &str) -> String {
    s.replace("\\n", "\n").replace("\\t", "\t")
}

fn words(action: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut chars = action.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' if !in_str => {
                in_str = true;
                cur.push(c);
            }
            '"' => {
                in_str = false;
                cur.push(c);
            }
            '\\' if in_str => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() && !in_str => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            '|' if !in_str => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push("|".into());
            }
            c => cur.push(c),
        }
    }
    if in_str {
        bail!(
            "unsupported template syntax {{{{{action}}}}}: a quote is not closed (see `fluxctl explain MAC_TEMPLATE_SYNTAX`)"
        );
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn operand(w: &str, action: &str) -> Result<Operand> {
    if w == "." {
        return Ok(Operand::Path(vec![]));
    }
    if let Some(p) = w.strip_prefix('.') {
        let segs: Vec<String> = p.split('.').map(str::to_owned).collect();
        if segs.iter().all(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
        }) {
            return Ok(Operand::Path(segs));
        }
        return Err(unsupported(&format!("{{{{{action}}}}}")));
    }
    if w.len() >= 2 && w.starts_with('"') && w.ends_with('"') {
        return Ok(Operand::Str(unescape_quoted(&w[1..w.len() - 1])));
    }
    if let Ok(n) = w.parse::<i64>() {
        return Ok(Operand::Int(n));
    }
    Err(unsupported(&format!("{{{{{action}}}}}")))
}

fn unescape_quoted(s: &str) -> String {
    s.replace("\\\"", "\"")
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\\\", "\\")
}

fn pipeline(action: &str, ws: &[String]) -> Result<Vec<Stage>> {
    let mut stages = Vec::new();
    for part in ws.split(|w| w == "|") {
        let Some(first) = part.first() else {
            return Err(unsupported(&format!("{{{{{action}}}}}")));
        };
        let func = match first.as_str() {
            "json" => Some(Func::Json),
            "index" => Some(Func::Index),
            _ => None,
        };
        match func {
            Some(f) => {
                let args = part[1..]
                    .iter()
                    .map(|w| operand(w, action))
                    .collect::<Result<Vec<_>>>()?;
                let ok = match f {
                    Func::Json => args.len() == 1 || (args.is_empty() && !stages.is_empty()),
                    Func::Index => args.len() >= 2 && stages.is_empty(),
                };
                if !ok {
                    bail!(
                        "unsupported template syntax {{{{{action}}}}}: `{first}` has the wrong number of arguments (use {{{{json .X}}}} or {{{{index .X \"key\"}}}}; see `fluxctl explain MAC_TEMPLATE_SYNTAX`)"
                    );
                }
                stages.push(Stage::Call(f, args));
            }
            None => {
                if part.len() != 1 || !stages.is_empty() {
                    return Err(unsupported(&format!("{{{{{action}}}}}")));
                }
                stages.push(Stage::Value(operand(first, action)?));
            }
        }
    }
    Ok(stages)
}

impl Template {
    pub fn parse(fmt: &str) -> Result<Template> {
        // Stack of open `range` blocks: (operand, nodes so far of the enclosing block).
        let mut stack: Vec<(Operand, Vec<Node>)> = Vec::new();
        let mut cur: Vec<Node> = Vec::new();
        let mut rest = fmt;
        while let Some(i) = rest.find("{{") {
            if i > 0 {
                cur.push(Node::Text(unescape(&rest[..i])));
            }
            let Some(j) = rest[i..].find("}}") else {
                bail!(
                    "--format {fmt:?}: a {{{{ is not closed with }}}} (see `fluxctl explain MAC_TEMPLATE_SYNTAX`)"
                );
            };
            let action = rest[i + 2..i + j].trim();
            rest = &rest[i + j + 2..];
            if action.starts_with('-') || action.ends_with('-') {
                return Err(unsupported(&format!("{{{{{action}}}}}")));
            }
            let ws = words(action)?;
            match ws.first().map(String::as_str) {
                None => return Err(unsupported("{{}}")),
                Some("range") => {
                    if ws.len() != 2 {
                        return Err(unsupported(&format!("{{{{{action}}}}}")));
                    }
                    let op = operand(&ws[1], action)?;
                    stack.push((op, std::mem::take(&mut cur)));
                }
                Some("end") => {
                    if ws.len() != 1 {
                        return Err(unsupported(&format!("{{{{{action}}}}}")));
                    }
                    let Some((op, outer)) = stack.pop() else {
                        bail!(
                            "--format {fmt:?}: {{{{end}}}} without a {{{{range}}}} (see `fluxctl explain MAC_TEMPLATE_SYNTAX`)"
                        );
                    };
                    let body = std::mem::replace(&mut cur, outer);
                    cur.push(Node::Range(op, body));
                }
                Some(
                    "if" | "else" | "with" | "define" | "template" | "block" | "break" | "continue"
                    | "printf" | "print" | "println" | "len" | "eq" | "ne" | "and" | "or" | "not"
                    | "html" | "js" | "slice",
                ) => {
                    return Err(unsupported(&format!("{{{{{action}}}}}")));
                }
                Some(_) => {
                    let stages = pipeline(action, &ws)?;
                    cur.push(Node::Action(action.to_owned(), stages));
                }
            }
        }
        if !stack.is_empty() {
            bail!(
                "--format {fmt:?}: a {{{{range}}}} has no {{{{end}}}} (see `fluxctl explain MAC_TEMPLATE_SYNTAX`)"
            );
        }
        if !rest.is_empty() {
            cur.push(Node::Text(unescape(rest)));
        }
        Ok(Template { nodes: cur })
    }

    /// Column names for `table`: the last path segment of each top-level `{{.A.B}}`, upper-cased.
    pub fn headers(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter_map(|n| match n {
                Node::Action(a, stages) => Some(match stages.first() {
                    Some(Stage::Value(Operand::Path(p))) if stages.len() == 1 && !p.is_empty() => {
                        p.last().cloned().unwrap_or_default().to_uppercase()
                    }
                    _ => a.to_uppercase(),
                }),
                _ => None,
            })
            .collect()
    }

    /// The template applied to one document.
    pub fn render(&self, doc: &Value) -> Result<String> {
        let mut out = String::new();
        render_nodes(&self.nodes, doc, &mut out)?;
        Ok(out)
    }
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    let o = v.as_object()?;
    if let Some(x) = o.get(name) {
        return Some(x);
    }
    let n = norm(name);
    o.iter().find(|(k, _)| norm(k) == n).map(|(_, x)| x)
}

fn eval_operand(op: &Operand, dot: &Value) -> Value {
    match op {
        Operand::Str(s) => Value::String(s.clone()),
        Operand::Int(n) => Value::from(*n),
        Operand::Path(p) => {
            let mut v = dot;
            for seg in p {
                match field(v, seg) {
                    Some(x) => v = x,
                    None => return Value::Null,
                }
            }
            v.clone()
        }
    }
}

fn index(base: &Value, keys: &[Value], action: &str) -> Result<Value> {
    let mut v = base.clone();
    for k in keys {
        v = match (&v, k) {
            (Value::Object(o), Value::String(s)) => o.get(s).cloned().unwrap_or(Value::Null),
            (Value::Array(a), Value::Number(n)) => {
                let i = n.as_i64().unwrap_or(-1);
                match usize::try_from(i).ok().and_then(|i| a.get(i)) {
                    Some(x) => x.clone(),
                    None => bail!(
                        "{{{{{action}}}}}: position {i} is outside the list (it has {} items)",
                        a.len()
                    ),
                }
            }
            (Value::Null, _) => Value::Null,
            _ => bail!(
                "{{{{{action}}}}}: `index` needs a text key for an object and a whole number for a list"
            ),
        };
    }
    Ok(v)
}

fn eval_pipeline(action: &str, stages: &[Stage], dot: &Value) -> Result<Value> {
    let mut prev: Option<Value> = None;
    for s in stages {
        prev = Some(match s {
            Stage::Value(o) => eval_operand(o, dot),
            Stage::Call(f, args) => {
                let mut vals: Vec<Value> = args.iter().map(|a| eval_operand(a, dot)).collect();
                // Only `json` can follow a pipe (`X | json`); the piped value is its argument.
                if let Some(p) = prev.take() {
                    vals.push(p);
                }
                match f {
                    Func::Json => Value::String(serde_json::to_string(&vals[0])?),
                    Func::Index => index(&vals[0], &vals[1..], action)?,
                }
            }
        });
    }
    Ok(prev.unwrap_or(Value::Null))
}

fn show(v: &Value) -> String {
    match v {
        Value::Null => "<no value>".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn render_nodes(nodes: &[Node], dot: &Value, out: &mut String) -> Result<()> {
    for n in nodes {
        match n {
            Node::Text(t) => out.push_str(t),
            Node::Action(a, stages) => out.push_str(&show(&eval_pipeline(a, stages, dot)?)),
            Node::Range(op, body) => match eval_operand(op, dot) {
                Value::Array(a) => {
                    for item in &a {
                        render_nodes(body, item, out)?;
                    }
                }
                Value::Object(o) => {
                    let mut keys: Vec<&String> = o.keys().collect();
                    keys.sort();
                    for k in keys {
                        render_nodes(body, &o[k], out)?;
                    }
                }
                Value::Null => {}
                other => bail!(
                    "range needs a list or an object, but that field is {} (see `fluxctl explain MAC_TEMPLATE_SYNTAX`)",
                    match other {
                        Value::String(_) => "text",
                        Value::Number(_) => "a number",
                        _ => "a boolean",
                    }
                ),
            },
        }
    }
    Ok(())
}

/// `--format` over many documents: one rendered line each, or, when the format starts with `table`, a header row and aligned
/// columns (the placeholders separated by a tab, written `\t`). `default_table` is used for a bare `table`.
pub fn render_rows(docs: &[Value], fmt: &str, default_table: &str) -> Result<String> {
    let (table, body) = match fmt.strip_prefix("table") {
        Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => (true, r.trim()),
        _ => (false, fmt),
    };
    if !table {
        let t = Template::parse(body)?;
        let mut out = String::new();
        for d in docs {
            out.push_str(&t.render(d)?);
            out.push('\n');
        }
        return Ok(out);
    }
    let t = Template::parse(if body.is_empty() { default_table } else { body })?;
    let mut grid = vec![t.headers()];
    for d in docs {
        grid.push(t.render(d)?.split('\t').map(str::to_owned).collect());
    }
    Ok(crate::mac_inspect::align_grid(&grid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({
            "name": "ci-1",
            "state": {"status": "running", "pid": 42, "error": null},
            "network": {"ip": "192.168.64.7", "ports": [{"host": 2222, "guest": 22}, {"host": 8080, "guest": 80}]},
            "resources": {"cpus": 4, "memory_mib": 8192},
            "labels": {"team": "ios", "env": "ci"}
        })
    }

    fn r(fmt: &str) -> String {
        Template::parse(fmt).unwrap().render(&doc()).unwrap()
    }

    #[test]
    fn field_paths_ignore_case_and_underscores() {
        assert_eq!(r("{{.State.Status}}"), "running");
        assert_eq!(r("{{.state.status}}"), "running");
        assert_eq!(r("{{.Network.IP}}"), "192.168.64.7");
        assert_eq!(r("{{.Resources.MemoryMiB}}"), "8192");
        assert_eq!(r("{{ .Name }}: {{.State.PID}}"), "ci-1: 42");
    }

    #[test]
    fn missing_and_null_print_no_value() {
        assert_eq!(r("{{.Nope}}"), "<no value>");
        assert_eq!(r("{{.State.Error}}"), "<no value>");
        assert_eq!(r("{{.State.Nope.Deeper}}"), "<no value>");
    }

    #[test]
    fn json_and_objects() {
        assert_eq!(
            r("{{json .State}}"),
            r#"{"status":"running","pid":42,"error":null}"#
        );
        assert_eq!(r("{{.State.Status | json}}"), r#""running""#);
        assert_eq!(r("{{.Resources}}"), r#"{"cpus":4,"memory_mib":8192}"#);
    }

    #[test]
    fn index_maps_and_arrays() {
        assert_eq!(r(r#"{{index .Labels "team"}}"#), "ios");
        assert_eq!(r(r#"{{index .Labels "absent"}}"#), "<no value>");
        assert_eq!(
            r("{{index .Network.Ports 1}}"),
            r#"{"host":8080,"guest":80}"#
        );
        assert!(r("{{index .Network.Ports 0 }}").contains("2222"));
        let e = Template::parse("{{index .Network.Ports 5}}")
            .unwrap()
            .render(&doc())
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("outside the list") && e.contains("2 items"),
            "{e}"
        );
    }

    #[test]
    fn range_rebinds_dot_and_nests() {
        assert_eq!(
            r(r"{{range .Network.Ports}}{{.Host}}->{{.Guest}} {{end}}"),
            "2222->22 8080->80 "
        );
        assert_eq!(r("{{range .Labels}}[{{.}}]{{end}}"), "[ci][ios]");
        assert_eq!(r("{{range .Missing}}x{{end}}done"), "done");
        assert_eq!(
            r("{{range .Network.Ports}}{{range .}}{{.}},{{end}};{{end}}"),
            "22,2222,;80,8080,;"
        );
    }

    #[test]
    fn escapes_in_text() {
        assert_eq!(r(r"{{.Name}}\t{{.State.Status}}\n"), "ci-1\trunning\n");
    }

    #[test]
    fn unsupported_syntax_names_what_is_supported() {
        for bad in [
            "{{if .Name}}x{{end}}",
            "{{printf \"%s\" .Name}}",
            "{{.Name | upper}}",
            "{{- .Name}}",
            "{{with .State}}x{{end}}",
            "{{len .Labels}}",
            "{{.Name}",
            "{{end}}",
            "{{range .Labels}}x",
            "{{}}",
            "{{.Name .State}}",
            "{{json}}",
            "{{index .Labels}}",
            "{{$x}}",
            "{{.a b}}",
        ] {
            let e = match Template::parse(bad) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("{bad} should be refused"),
            };
            assert!(e.contains("MAC_TEMPLATE_SYNTAX"), "{bad}: {e}");
        }
        let e = Template::parse("{{if .Name}}x{{end}}")
            .unwrap_err()
            .to_string();
        assert!(e.contains("{{json X}}") && e.contains("{{range X}}"), "{e}");
    }

    #[test]
    fn range_over_a_scalar_is_a_plain_error() {
        let t = Template::parse("{{range .Name}}x{{end}}").unwrap();
        let e = t.render(&doc()).unwrap_err().to_string();
        assert!(e.contains("range needs a list or an object"), "{e}");
    }

    #[test]
    fn rows_and_tables() {
        let docs = vec![doc(), json!({"name": "b", "state": {"status": "stopped"}})];
        assert_eq!(
            render_rows(&docs, "{{.Name}}={{.State.Status}}", "").unwrap(),
            "ci-1=running\nb=stopped\n"
        );
        let t = render_rows(&docs, r"table {{.Name}}\t{{.State.Status}}", "").unwrap();
        assert_eq!(t, "NAME  STATUS\nci-1  running\nb     stopped\n");
        let d = render_rows(&docs, "table", r"{{.Name}}\t{{.Network.IP}}").unwrap();
        assert!(
            d.starts_with("NAME  IP\n") && d.contains("b     <no value>"),
            "{d}"
        );
    }
}
