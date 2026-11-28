//! The telemetry tables (docs/owner.md §3): one definition per table, its rows keyed by a
//! stable id, the plain HTML table every page carries (the no-script view, which the
//! script reads its columns from), and the row diffs the live feed sends.
//!
//! Live feed (`/telemetry/events`, server-sent events), each `data` one JSON object:
//!
//! - `reset`: `{"meta": Meta, "tables": {"<table>": [row, ...]}}`, every table, first
//!   on each connection;
//! - `diff`: `{"meta": Meta, "tables": {"<table>": {"add": [row], "update": [row],
//!   "remove": ["<id>"]}}}`, only the tables that changed.
//!
//! A row is a flat JSON object whose `id` is unique within its table and stays the same
//! while the row lives; the page applies a diff as one AG Grid transaction by `id`.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::html::esc;

/// How a column is shown, filtered and sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    /// Long text that wraps.
    Long,
    /// A hash or id, in monospace.
    Code,
    Num,
    /// A position (no highlight when it moves).
    Rank,
    /// Minutes, shown as `3d`, `4h`, `12m` (no highlight as it grows).
    Age,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Long => "long",
            Kind::Code => "code",
            Kind::Num => "num",
            Kind::Rank => "rank",
            Kind::Age => "age",
        }
    }
}

pub struct Column {
    pub field: &'static str,
    pub header: &'static str,
    pub kind: Kind,
}

const fn col(field: &'static str, header: &'static str, kind: Kind) -> Column {
    Column {
        field,
        header,
        kind,
    }
}

pub struct TableDef {
    pub id: &'static str,
    pub caption: &'static str,
    pub empty: &'static str,
    /// The initial sort: a field and `asc` or `desc`.
    pub sort: Option<(&'static str, &'static str)>,
    pub columns: &'static [Column],
}

/// A tab of the page and its tables.
pub struct Tab {
    pub id: &'static str,
    pub label: &'static str,
    pub tables: &'static [&'static TableDef],
    /// The tab's label shows the row count of its first table.
    pub count: bool,
}

use Kind::*;

pub static HOSTS: TableDef = TableDef {
    id: "hosts",
    caption: "Live hosts (a host record lapses two minutes after its bridge stops)",
    empty: "No live host.",
    sort: None,
    columns: &[
        col("host", "Host", Text),
        col("seat", "Seat", Text),
        col("role", "Role", Text),
        col("runtime", "Runtime", Text),
        col("model", "Model", Text),
        col("memcap_gb", "Memory cap (GB)", Num),
        col("free_gb", "Free disk (GB)", Num),
    ],
};

pub static TASKS: TableDef = TableDef {
    id: "tasks",
    caption: "Registered worktrees",
    empty: "No task in flight.",
    sort: None,
    columns: &[
        col("seat", "Seat", Text),
        col("task", "Issue", Text),
        col("work", "Work", Text),
        col("stage", "Stage", Text),
        col("last", "Last note", Text),
        col("last_at", "Last note at", Text),
        col("host", "Host", Text),
        col("age", "Age", Age),
        col("stale", "Stale", Text),
    ],
};

pub static PRS: TableDef = TableDef {
    id: "prs",
    caption: "Open pull requests",
    empty: "None open.",
    sort: None,
    columns: &[
        col("pr", "Pull request", Text),
        col("head", "Head", Code),
        col("state", "State", Text),
        col("bot", "Bot", Text),
        col("threads", "Threads", Num),
        col("checks", "Checks", Text),
    ],
};

pub static QUEUE: TableDef = TableDef {
    id: "queue",
    caption: "Claimable issues in order, priority first",
    empty: "Nothing claimable.",
    sort: Some(("pos", "asc")),
    columns: &[
        col("pos", "#", Rank),
        col("issue", "Issue", Text),
        col("class", "Class", Text),
        col("opened", "Opened", Text),
        col("title", "Title", Long),
    ],
};

pub static LABELS: TableDef = TableDef {
    id: "labels",
    caption: "Open issues by label and repository",
    empty: "No backlog reading yet.",
    sort: None,
    columns: &[
        col("repo", "Repository", Text),
        col("label", "Label", Text),
        col("open", "Open", Num),
    ],
};

pub static BURNDOWN: TableDef = TableDef {
    id: "burndown",
    caption: "Burndown: open, opened, closed and net per day",
    empty: "No samples.",
    sort: Some(("day", "desc")),
    columns: &[
        col("repo", "Repository", Text),
        col("day", "Day", Text),
        col("open", "Open", Num),
        col("opened", "Opened", Num),
        col("closed", "Closed", Num),
        col("net", "Net", Num),
    ],
};

pub static LANDINGS: TableDef = TableDef {
    id: "landings",
    caption: "The newest landings first",
    empty: "Nothing landed yet.",
    sort: Some(("when", "desc")),
    columns: &[
        col("when", "When", Text),
        col("pr", "Pull request", Text),
        col("merge", "Merge", Code),
    ],
};

pub static FLEET: TableDef = TableDef {
    id: "fleet",
    caption: "Mode, hold, stale trees and open questions",
    empty: "No reading yet.",
    sort: None,
    columns: &[col("item", "Item", Text), col("value", "Value", Long)],
};

pub static RUNS: TableDef = TableDef {
    id: "runs",
    caption: "Recent build runs",
    empty: "No build run kept.",
    sort: Some(("started", "desc")),
    columns: &[
        col("run", "Run", Code),
        col("repo", "Repository", Text),
        col("head", "Head", Code),
        col("host", "Builder", Text),
        col("platform", "Platform", Text),
        col("status", "Status", Text),
        col("started", "Started", Text),
        col("wall_s", "Wall (s)", Num),
        col("errors", "Errors", Num),
        col("passed", "Passed", Num),
        col("failed", "Failed", Num),
    ],
};

pub static BUILDERS: TableDef = TableDef {
    id: "builders",
    caption: "Live builders",
    empty: "No live builder.",
    sort: None,
    columns: &[
        col("builder", "Builder", Text),
        col("platform", "Platform", Text),
        col("arch", "Arch", Text),
        col("executor", "Executor", Text),
        col("state", "State", Text),
        col("reachable", "Reachable", Text),
        col("busy", "Busy", Num),
        col("slots", "Slots", Num),
        col("free_gb", "Free disk (GB)", Num),
        col("toolchains", "Toolchains", Long),
        col("updated", "Updated", Text),
    ],
};

pub static USAGE: TableDef = TableDef {
    id: "usage",
    caption: "Subscription usage, one row per limit of each login",
    empty: "No usage record.",
    sort: None,
    columns: &[
        col("login", "Login", Text),
        col("runtime", "Runtime", Text),
        col("host", "Host", Text),
        col("plan", "Plan", Text),
        col("scope", "Scope", Text),
        col("window", "Window", Text),
        col("used_pct", "Used %", Num),
        col("resets", "Resets", Text),
        col("observed", "Observed", Text),
        col("limit", "Limit reached", Text),
    ],
};

pub static TABS: &[Tab] = &[
    Tab {
        id: "hosts",
        label: "Hosts and seats",
        tables: &[&HOSTS],
        count: false,
    },
    Tab {
        id: "tasks",
        label: "Tasks",
        tables: &[&TASKS],
        count: true,
    },
    Tab {
        id: "prs",
        label: "Pull requests",
        tables: &[&PRS],
        count: true,
    },
    Tab {
        id: "queue",
        label: "Queue",
        tables: &[&QUEUE],
        count: true,
    },
    Tab {
        id: "backlog",
        label: "Backlog",
        tables: &[&LABELS, &BURNDOWN],
        count: false,
    },
    Tab {
        id: "landings",
        label: "Landings",
        tables: &[&LANDINGS],
        count: false,
    },
    Tab {
        id: "fleet",
        label: "Fleet",
        tables: &[&FLEET],
        count: false,
    },
    Tab {
        id: "builds",
        label: "Builds",
        tables: &[&RUNS, &BUILDERS],
        count: false,
    },
    Tab {
        id: "usage",
        label: "Usage",
        tables: &[&USAGE],
        count: false,
    },
];

pub type Row = Map<String, Value>;

/// Every table's rows, in display order, by table id.
pub type Tables = BTreeMap<&'static str, Vec<Row>>;

/// A row from `id` and its fields.
pub fn row(id: impl Into<String>, fields: impl IntoIterator<Item = (&'static str, Value)>) -> Row {
    let mut r = Map::new();
    r.insert("id".into(), Value::String(id.into()));
    for (k, v) in fields {
        r.insert(k.into(), v);
    }
    r
}

/// The text the page shows above the tables, and per tab.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Meta {
    /// `Read <time>`, and what could not be read.
    pub read: String,
    pub error: Option<String>,
    /// A line above a tab's tables (`prs`: when the table was read).
    pub notes: BTreeMap<&'static str, String>,
    /// Row counts shown in tab labels.
    pub counts: BTreeMap<&'static str, usize>,
}

/// The changes to one table.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TableDiff {
    pub add: Vec<Row>,
    pub update: Vec<Row>,
    pub remove: Vec<String>,
}

impl TableDiff {
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.update.is_empty() && self.remove.is_empty()
    }
}

fn id_of(r: &Row) -> &str {
    r.get("id").and_then(Value::as_str).unwrap_or("")
}

/// The changes from `prev` to `next`, by row id; tables without a change are left out.
/// Removals keep `prev`'s order, additions and updates `next`'s.
pub fn diff(prev: &Tables, next: &Tables) -> BTreeMap<&'static str, TableDiff> {
    let mut out = BTreeMap::new();
    let none = Vec::new();
    let names = prev.keys().chain(next.keys());
    for name in names {
        if out.contains_key(name) {
            continue;
        }
        let (p, n) = (
            prev.get(name).unwrap_or(&none),
            next.get(name).unwrap_or(&none),
        );
        let before: HashMap<&str, &Row> = p.iter().map(|r| (id_of(r), r)).collect();
        let after: HashMap<&str, &Row> = n.iter().map(|r| (id_of(r), r)).collect();
        let mut d = TableDiff::default();
        for r in n {
            match before.get(id_of(r)) {
                None => d.add.push(r.clone()),
                Some(old) if *old != r => d.update.push(r.clone()),
                Some(_) => {}
            }
        }
        for r in p {
            if !after.contains_key(id_of(r)) {
                d.remove.push(id_of(r).to_string());
            }
        }
        if !d.is_empty() {
            out.insert(*name, d);
        }
    }
    out
}

#[derive(Serialize)]
struct Reset<'a> {
    meta: &'a Meta,
    tables: &'a Tables,
}

#[derive(Serialize)]
struct Diff<'a> {
    meta: &'a Meta,
    tables: &'a BTreeMap<&'static str, TableDiff>,
}

/// The `data` of a `reset` event.
pub fn reset_json(meta: &Meta, tables: &Tables) -> String {
    serde_json::to_string(&Reset { meta, tables }).unwrap_or_else(|_| "{}".into())
}

/// The `data` of a `diff` event.
pub fn diff_json(meta: &Meta, d: &BTreeMap<&'static str, TableDiff>) -> String {
    serde_json::to_string(&Diff { meta, tables: d }).unwrap_or_else(|_| "{}".into())
}

/// Minutes as the page shows them (the script's `age` matches).
pub fn age(min: i64) -> String {
    let m = min.max(0);
    if m >= 1440 {
        format!("{}d", m / 1440)
    } else if m >= 60 {
        format!("{}h", m / 60)
    } else {
        format!("{m}m")
    }
}

fn cell(kind: Kind, v: Option<&Value>) -> String {
    match (kind, v) {
        (_, None | Some(Value::Null)) => String::new(),
        (Age, Some(Value::Number(n))) => age(n.as_i64().unwrap_or(0)),
        (Code, Some(Value::String(s))) => format!("<code>{}</code>", esc(s)),
        (_, Some(Value::String(s))) => esc(s),
        (_, Some(v)) => esc(&v.to_string()),
    }
}

/// One table as plain HTML: the no-script view, and the column list the script reads.
pub fn html_table(def: &TableDef, rows: &[Row]) -> String {
    let mut s = format!("<table data-grid=\"{}\"", def.id);
    if let Some((f, d)) = def.sort {
        let _ = write!(s, " data-sort=\"{f} {d}\"");
    }
    let _ = write!(
        s,
        " data-empty=\"{}\"><caption>{}</caption><thead><tr>",
        esc(def.empty),
        esc(def.caption)
    );
    for c in def.columns {
        let _ = write!(
            s,
            "<th scope=\"col\" data-field=\"{}\" data-kind=\"{}\">{}</th>",
            c.field,
            c.kind.as_str(),
            esc(c.header)
        );
    }
    s.push_str("</tr></thead><tbody>");
    if rows.is_empty() {
        let _ = write!(
            s,
            "<tr><td colspan=\"{}\">{}</td></tr>",
            def.columns.len(),
            esc(def.empty)
        );
    }
    for r in rows {
        s.push_str("<tr>");
        for c in def.columns {
            let _ = write!(s, "<td>{}</td>", cell(c.kind, r.get(c.field)));
        }
        s.push_str("</tr>");
    }
    s.push_str("</tbody></table>");
    s
}

/// The tabs and their panels. Without the script every panel shows, one after another.
pub fn html_tabs(meta: &Meta, tables: &Tables) -> String {
    let mut o = String::new();
    let _ = write!(o, "<p class=\"meta\" id=\"read\">{}", esc(&meta.read));
    o.push_str("</p>");
    let _ = write!(
        o,
        "<p class=\"meta warn\" id=\"read-error\"{}>{}</p>",
        if meta.error.is_some() { "" } else { " hidden" },
        esc(meta.error.as_deref().unwrap_or(""))
    );
    o.push_str("<nav class=\"tabs\" aria-label=\"Telemetry\"><ul>");
    for t in TABS {
        let n = match meta.counts.get(t.id) {
            Some(n) if t.count => format!(" <span class=\"count\">{n}</span>"),
            _ => String::new(),
        };
        let _ = write!(
            o,
            "<li><a href=\"#{id}\" id=\"tab-{id}\" data-tab=\"{id}\">{}{n}</a></li>",
            esc(t.label),
            id = t.id
        );
    }
    o.push_str("</ul></nav>");
    let none = Vec::new();
    for t in TABS {
        let _ = write!(
            o,
            "<section class=\"panel\" id=\"{id}\" aria-labelledby=\"h-{id}\"><h2 id=\"h-{id}\">{}</h2>",
            esc(t.label),
            id = t.id
        );
        let note = meta.notes.get(t.id);
        let _ = write!(
            o,
            "<p class=\"meta\" data-note=\"{}\"{}>{}</p>",
            t.id,
            if note.is_some() { "" } else { " hidden" },
            esc(note.map(String::as_str).unwrap_or(""))
        );
        for d in t.tables {
            o.push_str(&html_table(d, tables.get(d.id).unwrap_or(&none)));
        }
        o.push_str("</section>");
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashSet;

    fn t(rows: Vec<Row>) -> Tables {
        let mut m = Tables::new();
        m.insert("prs", rows);
        m
    }

    #[test]
    fn diffs_by_id() {
        let a = row("alpha#1", [("checks", json!("pass"))]);
        let b = row("alpha#2", [("checks", json!("pending"))]);
        let b2 = row("alpha#2", [("checks", json!("fail"))]);
        let c = row("beta#3", [("checks", json!("pass"))]);
        let d = diff(&t(vec![a.clone(), b]), &t(vec![b2.clone(), c.clone()]));
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({"prs": {
                "add": [{"id": "beta#3", "checks": "pass"}],
                "update": [{"id": "alpha#2", "checks": "fail"}],
                "remove": ["alpha#1"],
            }})
        );
        assert!(diff(&t(vec![a.clone()]), &t(vec![a.clone()])).is_empty());
        // A table that appears or goes is all additions or removals.
        let d = diff(&Tables::new(), &t(vec![a.clone()]));
        assert_eq!(d["prs"].add, vec![a.clone()]);
        let d = diff(&t(vec![a]), &Tables::new());
        assert_eq!(d["prs"].remove, vec!["alpha#1".to_string()]);
        let meta = Meta {
            read: "Read x".into(),
            ..Meta::default()
        };
        let v: Value = serde_json::from_str(&diff_json(&meta, &d)).unwrap();
        assert_eq!(v["tables"]["prs"]["remove"], json!(["alpha#1"]));
        assert_eq!(v["meta"]["read"], json!("Read x"));
        let v: Value = serde_json::from_str(&reset_json(&meta, &t(vec![c]))).unwrap();
        assert_eq!(v["tables"]["prs"][0]["id"], json!("beta#3"));
    }

    #[test]
    fn tables_and_tabs() {
        let (mut tabs, mut tables) = (HashSet::new(), HashSet::new());
        for t in TABS {
            assert!(tabs.insert(t.id), "tab {}", t.id);
            for d in t.tables {
                assert!(tables.insert(d.id), "table {}", d.id);
                if let Some((f, dir)) = d.sort {
                    assert!(d.columns.iter().any(|c| c.field == f), "{}", d.id);
                    assert!(dir == "asc" || dir == "desc");
                }
                assert!(d.columns.iter().all(|c| c.field != "id"), "{}", d.id);
            }
        }
        assert_eq!(
            (age(5), age(125), age(3 * 1440 + 7), age(-3)),
            ("5m".into(), "2h".into(), "3d".into(), "0m".into())
        );
    }

    #[test]
    fn html_escapes_and_carries_columns() {
        let r = row(
            "q",
            [
                ("pos", json!(1)),
                ("title", json!("<script>x</script>")),
                ("issue", json!("a#1")),
            ],
        );
        let h = html_table(&QUEUE, &[r]);
        assert!(h.contains("&lt;script&gt;x&lt;/script&gt;"));
        assert!(!h.contains("<script>"));
        assert!(h.contains("data-grid=\"queue\" data-sort=\"pos asc\""));
        assert!(h.contains("<th scope=\"col\" data-field=\"title\" data-kind=\"long\">Title</th>"));
        let h = html_table(&PRS, &[]);
        assert!(h.contains("<td colspan=\"6\">None open.</td>"));
    }
}
