//! Parsers of the three sources (docs/protocol.md PROTO-23). The claude and agy outputs
//! are display text, not an API: a line that does not parse is kept as it was, never an
//! error. Pure functions; `now` is a parameter.

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use hgl_ai_proto::usage::{MAX_UNPARSED, MAX_UNPARSED_CHARS};
use hgl_ai_proto::{UsageBucket, utc_stamp};
use regex::Regex;
use serde::Deserialize;

/// Buckets read from a source, and the lines that did not parse.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub buckets: Vec<UsageBucket>,
    pub unparsed: Vec<String>,
}

impl Parsed {
    fn keep(&mut self, line: &str) {
        if self.unparsed.len() < MAX_UNPARSED {
            self.unparsed.push(clip(line));
        }
    }

    /// No bucket at all: keep the text itself, so `usage show` says what the source said.
    fn keep_all_if_empty(&mut self, text: &str) {
        if self.buckets.is_empty() && self.unparsed.is_empty() {
            for l in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
                self.keep(l);
            }
        }
    }
}

fn clip(line: &str) -> String {
    line.trim().chars().take(MAX_UNPARSED_CHARS).collect()
}

fn pct(s: &str) -> Option<f64> {
    s.trim()
        .trim_end_matches('%')
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|p| p.is_finite())
        .map(|p| p.clamp(0.0, 100.0))
}

/// A window name: `5h`, `week`, or `<minutes>m`.
pub fn window_of_minutes(m: u64) -> String {
    match m {
        300 => "5h".into(),
        10080 => "week".into(),
        _ => format!("{m}m"),
    }
}

// ---- claude -------------------------------------------------------------------------

/// The account fields of `claude auth status` (JSON); nothing else is read.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeAuth {
    #[serde(default)]
    logged_in: bool,
    org_id: Option<String>,
    email: Option<String>,
    plan_tier: Option<String>,
    subscription_type: Option<String>,
}

/// The login of `claude auth status`: its account id (`orgId/email`) and plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    pub id: String,
    pub plan: Option<String>,
}

/// `Err` says why there is no login (not signed in, unreadable output).
pub fn claude_login(json: &str) -> Result<Login, String> {
    let a: ClaudeAuth = serde_json::from_str(json)
        .map_err(|e| format!("`claude auth status` is not the JSON expected: {e}"))?;
    if !a.logged_in {
        return Err("claude is not signed in".into());
    }
    let nonempty = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
    let id = match (nonempty(a.org_id), nonempty(a.email)) {
        (Some(o), Some(e)) => format!("{o}/{e}"),
        (Some(x), None) | (None, Some(x)) => x,
        (None, None) => return Err("`claude auth status` names no orgId or email".into()),
    };
    Ok(Login {
        id,
        plan: nonempty(a.plan_tier).or(nonempty(a.subscription_type)),
    })
}

static CLAUDE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^Current (?P<what>week|session)(?: \((?P<scope>[^)]*)\))?\s*:\s*(?P<pct>\d+(?:\.\d+)?)\s*% used(?:\s*·\s*resets\s+(?P<reset>.+?))?\s*$",
    )
    .unwrap()
});

static CLAUDE_RESET: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:(?P<mon>[A-Za-z]{3})[A-Za-z]*\.? (?P<day>\d{1,2}),?\s+(?:at\s+)?)?(?P<h>\d{1,2})(?::(?P<m>\d{2}))?\s*(?P<ap>am|pm|AM|PM)\s*(?:\((?P<tz>[^)]+)\))?$",
    )
    .unwrap()
});

/// `claude -p "/usage"`: one bucket per `Current week|session (...)` line; the usage
/// breakdown after `What's contributing` is ignored.
pub fn claude_usage(text: &str, now: DateTime<Utc>) -> Parsed {
    let mut p = Parsed::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("What's contributing") || line.starts_with("What’s contributing") {
            break;
        }
        if line.is_empty() {
            continue;
        }
        let Some(c) = CLAUDE_LINE.captures(line) else {
            if line.starts_with("Current") || line.contains('%') {
                p.keep(line);
            }
            continue;
        };
        let Some(used_pct) = pct(&c["pct"]) else {
            p.keep(line);
            continue;
        };
        let scope = match c.name("scope").map(|m| m.as_str().trim()) {
            None | Some("all models") | Some("") => "all".to_string(),
            Some(s) => s.to_lowercase(),
        };
        let window = if &c["what"] == "session" {
            "5h"
        } else {
            "week"
        }
        .to_string();
        let resets_at = match c.name("reset") {
            None => None,
            Some(r) => {
                let t = claude_reset(r.as_str(), now);
                if t.is_none() {
                    p.keep(line);
                }
                t.map(utc_stamp)
            }
        };
        p.buckets.push(UsageBucket {
            scope,
            window,
            used_pct,
            resets_at,
        });
    }
    p.keep_all_if_empty(text);
    p
}

/// A claude reset time: `Dec 4, 9:59am (Asia/Dubai)` or `3pm (Asia/Dubai)`, in the zone
/// named (the host's when none is). No year: the first such time from a day before `now`;
/// no date: the next such time after `now`.
pub fn claude_reset(s: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let c = CLAUDE_RESET.captures(s.trim())?;
    let tz = match c.name("tz") {
        Some(z) => jiff::tz::TimeZone::get(z.as_str().trim()).ok()?,
        None => jiff::tz::TimeZone::system(),
    };
    let mut hour: i8 = c["h"].parse().ok()?;
    let minute: i8 = c.name("m").map_or(Some(0), |m| m.as_str().parse().ok())?;
    if !(1..=12).contains(&hour) || !(0..60).contains(&minute) {
        return None;
    }
    let pm = c["ap"].eq_ignore_ascii_case("pm");
    hour = match (hour, pm) {
        (12, false) => 0,
        (12, true) => 12,
        (h, true) => h + 12,
        (h, false) => h,
    };
    let now_j = jiff::Timestamp::from_second(now.timestamp()).ok()?;
    let today = now_j.to_zoned_datetime(tz.clone()).date();
    let at = |d: jiff::civil::Date| -> Option<i64> {
        Some(
            d.at(hour, minute, 0, 0)
                .to_zoned_datetime(tz.clone())
                .ok()?
                .timestamp()
                .as_second(),
        )
    };
    let secs = match (c.name("mon"), c.name("day")) {
        (Some(mon), Some(day)) => {
            let month = month_of(mon.as_str())?;
            let day: i8 = day.as_str().parse().ok()?;
            // The earliest of last, this and next year's that is not over a day past.
            [today.year() - 1, today.year(), today.year() + 1]
                .into_iter()
                .filter_map(|y| at(jiff::civil::Date::new(y, month, day).ok()?))
                .find(|t| *t >= now.timestamp() - 86_400)?
        }
        _ => {
            let t = at(today)?;
            if t >= now.timestamp() {
                t
            } else {
                at(today.tomorrow().ok()?)?
            }
        }
    };
    DateTime::from_timestamp(secs, 0)
}

fn month_of(s: &str) -> Option<i8> {
    const M: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let l = s.to_ascii_lowercase();
    M.iter().position(|m| *m == l).map(|i| i as i8 + 1)
}

// ---- agy ----------------------------------------------------------------------------

/// `agy -p "/usage"`: `<bucket>\t<window> Limit Remaining\t<pct>%\t<reset RFC 3339>`.
/// A reset at the epoch is a window not started.
pub fn agy_usage(text: &str) -> Parsed {
    let mut p = Parsed::default();
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').map(str::trim).collect();
        let parsed = (f.len() == 4)
            .then(|| {
                let window = f[1].strip_suffix(" Limit Remaining")?;
                let remaining = pct(f[2])?;
                let reset = DateTime::parse_from_rfc3339(f[3]).ok()?.with_timezone(&Utc);
                Some(UsageBucket {
                    scope: agy_scope(f[0]),
                    window: match window {
                        "Five Hour" => "5h".to_string(),
                        "Weekly" => "week".to_string(),
                        w => w.to_lowercase().replace(' ', "-"),
                    },
                    used_pct: 100.0 - remaining,
                    resets_at: (reset.timestamp() > 0).then(|| utc_stamp(reset)),
                })
            })
            .flatten();
        match parsed {
            Some(b) => p.buckets.push(b),
            None => p.keep(line),
        }
    }
    p.keep_all_if_empty(text);
    p
}

fn agy_scope(s: &str) -> String {
    match s {
        "Gemini Models" => "gemini".into(),
        "Claude and GPT models" => "claude+gpt".into(),
        other => other
            .to_lowercase()
            .trim_end_matches(" models")
            .replace(" and ", "+")
            .replace(' ', "-"),
    }
}

/// The signed-in account of an agy log: the last `applyAuthResult: email=<id>,` line.
pub fn agy_account(log: &str) -> Option<String> {
    log.lines()
        .filter_map(|l| l.split_once("applyAuthResult: email=").map(|(_, r)| r))
        .filter_map(|r| r.split([',', ' ']).next())
        .rfind(|id| !id.trim().is_empty())
        .map(|id| id.trim().to_string())
}

// ---- codex --------------------------------------------------------------------------

/// The one field of `~/.codex/auth.json` read: `tokens.account_id`. Every other field,
/// the tokens among them, is skipped by the parser and never kept.
#[derive(Deserialize)]
struct CodexAuth {
    tokens: Option<CodexTokens>,
}

#[derive(Deserialize)]
struct CodexTokens {
    account_id: Option<String>,
}

pub fn codex_account(json: &str) -> Result<String, String> {
    let a: CodexAuth =
        serde_json::from_str(json).map_err(|e| format!("codex auth.json does not parse: {e}"))?;
    a.tokens
        .and_then(|t| t.account_id)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            "codex is not signed in with a ChatGPT account (no tokens.account_id)".into()
        })
}

/// The newest `rate_limits` of a Codex rollout.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexLimits {
    /// The event's own time.
    pub observed_at: DateTime<Utc>,
    pub plan: Option<String>,
    pub limit_reached: Option<String>,
    pub parsed: Parsed,
}

/// What a rollout says about the account: its `session_meta`'s `creator_account_id`, if
/// any, and its last `rate_limits`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rollout {
    pub account_id: Option<String>,
    pub limits: Option<CodexLimits>,
}

pub fn codex_rollout(text: &str) -> Rollout {
    let mut out = Rollout::default();
    for line in text.lines() {
        if out.account_id.is_none() && line.contains("\"session_meta\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                && v["type"] == "session_meta"
            {
                out.account_id = v["payload"]["creator_account_id"]
                    .as_str()
                    .map(String::from);
            }
            continue;
        }
        if !line.contains("\"rate_limits\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let rl = &v["payload"]["rate_limits"];
        if !rl.is_object() {
            continue;
        }
        let Some(at) = v["timestamp"]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        else {
            continue;
        };
        out.limits = Some(codex_limits(rl, at.with_timezone(&Utc)));
    }
    out
}

fn codex_limits(rl: &serde_json::Value, observed_at: DateTime<Utc>) -> CodexLimits {
    let mut parsed = Parsed::default();
    let scope = rl["limit_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map_or_else(|| "all".to_string(), str::to_lowercase);
    for which in ["primary", "secondary"] {
        let w = &rl[which];
        if w.is_null() {
            continue;
        }
        let bucket = (|| {
            Some(UsageBucket {
                scope: scope.clone(),
                window: window_of_minutes(w["window_minutes"].as_u64()?),
                used_pct: w["used_percent"].as_f64()?.clamp(0.0, 100.0),
                resets_at: w["resets_at"]
                    .as_i64()
                    .filter(|s| *s > 0)
                    .and_then(|s| DateTime::from_timestamp(s, 0))
                    .map(utc_stamp),
            })
        })();
        match bucket {
            Some(b) => parsed.buckets.push(b),
            None => parsed.keep(&format!("{which}: {w}")),
        }
    }
    CodexLimits {
        observed_at,
        plan: rl["plan_type"].as_str().map(String::from),
        limit_reached: rl["rate_limit_reached_type"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from),
        parsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn b(scope: &str, window: &str, used: f64, resets: Option<&str>) -> UsageBucket {
        UsageBucket {
            scope: scope.into(),
            window: window.into(),
            used_pct: used,
            resets_at: resets.map(String::from),
        }
    }

    #[test]
    fn claude_captured() {
        let p = claude_usage(&fixture("claude-usage.txt"), at("2026-11-27T17:58:00Z"));
        // Dec 4, 10am in Asia/Dubai (UTC+4) is 06:00 UTC.
        assert_eq!(
            p.buckets,
            vec![
                b("all", "week", 20.0, Some("20261204T060000Z")),
                b("fable", "week", 2.0, Some("20261204T060000Z")),
            ]
        );
        assert!(p.unparsed.is_empty(), "{:?}", p.unparsed);
    }

    #[test]
    fn claude_session_and_a_line_that_does_not_parse() {
        let p = claude_usage(
            &fixture("claude-usage-session.synthetic.txt"),
            at("2026-11-27T17:58:00Z"),
        );
        // 3pm Dubai today (11:00 UTC) has passed at 17:58 UTC: tomorrow's.
        assert_eq!(
            p.buckets,
            vec![
                b("all", "5h", 35.0, Some("20261128T110000Z")),
                b("all", "week", 20.0, Some("20261204T055900Z")),
                b("fable", "week", 2.0, Some("20261204T060000Z")),
            ]
        );
        assert_eq!(
            p.unparsed,
            vec!["Current week (Opus): nearly there · resets soon"]
        );
    }

    #[test]
    fn claude_resets() {
        let now = at("2026-12-30T12:00:00Z");
        // No year: a date before today is next year's.
        assert_eq!(
            claude_reset("Jan 2, 12am (UTC)", now).map(utc_stamp),
            Some("20270102T000000Z".into())
        );
        assert_eq!(
            claude_reset("Dec 30, 12pm (UTC)", now).map(utc_stamp),
            Some("20261230T120000Z".into())
        );
        assert_eq!(
            claude_reset("1:30pm (Europe/London)", now).map(utc_stamp),
            Some("20261230T133000Z".into())
        );
        // r1 minor 3: on Jan 1, a reset just before midnight is last year's, not next Dec 31.
        assert_eq!(
            claude_reset("Dec 31, 11pm (UTC)", at("2027-01-01T00:30:00Z")).map(utc_stamp),
            Some("20261231T230000Z".into())
        );
        assert_eq!(
            claude_reset("Jan 1, 9am (UTC)", at("2026-12-31T20:00:00Z")).map(utc_stamp),
            Some("20270101T090000Z".into())
        );
        assert_eq!(claude_reset("3pm (Mars/Olympus)", now), None);
        assert_eq!(claude_reset("soon", now), None);
        assert_eq!(claude_reset("13pm (UTC)", now), None);
    }

    #[test]
    fn claude_text_without_buckets_is_kept() {
        let p = claude_usage("Not logged in · Please run /login\n", Utc::now());
        assert!(p.buckets.is_empty());
        assert_eq!(p.unparsed, vec!["Not logged in · Please run /login"]);
    }

    #[test]
    fn claude_logins() {
        let l = claude_login(&fixture("claude-auth.json")).unwrap();
        assert_eq!(
            l,
            Login {
                id: "00000000-0000-4000-8000-000000000001/someone@example.com".into(),
                plan: Some("max_20x".into()),
            }
        );
        assert!(claude_login(r#"{"loggedIn": false}"#).is_err());
        assert!(claude_login("not json").is_err());
        assert_eq!(
            claude_login(r#"{"loggedIn": true, "email": "a@b", "subscriptionType": "pro"}"#)
                .unwrap(),
            Login {
                id: "a@b".into(),
                plan: Some("pro".into())
            }
        );
    }

    #[test]
    fn agy_captured() {
        let p = agy_usage(&fixture("agy-usage.txt"));
        assert_eq!(
            p.buckets,
            vec![
                b("gemini", "week", 19.0, Some("20261203T135517Z")),
                b("gemini", "5h", 10.0, Some("20261127T195517Z")),
                b("claude+gpt", "week", 10.0, Some("20261204T101428Z")),
                b("claude+gpt", "5h", 0.0, None),
            ]
        );
        assert!(p.unparsed.is_empty());
        let p = agy_usage("Gemini Models\tWeekly Limit Remaining\tlots\t2026-12-03T13:55:17Z\nX\n");
        assert!(p.buckets.is_empty());
        assert_eq!(p.unparsed.len(), 2);
        assert_eq!(agy_scope("Other Models"), "other");
    }

    #[test]
    fn agy_accounts() {
        assert_eq!(
            agy_account(&fixture("agy-cli.log")).as_deref(),
            Some("someone@example.com")
        );
        let two = "x applyAuthResult: email=a@b, authMethod=consumer\ny applyAuthResult: email=c@d, authMethod=consumer\n";
        assert_eq!(agy_account(two).as_deref(), Some("c@d"));
        assert_eq!(agy_account("no sign-in here\n"), None);
    }

    #[test]
    fn codex_captured() {
        let r = codex_rollout(&fixture("codex-rollout.jsonl"));
        assert_eq!(r.account_id.as_deref(), Some("acct-fixture-0001"));
        let l = r.limits.unwrap();
        assert_eq!(l.observed_at, at("2026-11-27T17:16:50.582Z"));
        assert_eq!(l.plan.as_deref(), Some("promax"));
        assert_eq!(l.limit_reached, None);
        assert_eq!(
            l.parsed.buckets,
            vec![b("all", "week", 7.0, Some("20261204T061705Z"))]
        );
        assert!(l.parsed.unparsed.is_empty());
    }

    #[test]
    fn codex_both_windows_and_odd_ones() {
        let line = r#"{"timestamp":"2026-11-27T10:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":41.5,"window_minutes":300,"resets_at":1796300000},"secondary":{"used_percent":12,"window_minutes":10080,"resets_at":0},"plan_type":"pro","rate_limit_reached_type":"primary"}}}
{"timestamp":"2026-11-27T11:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":null}}
{"timestamp":"2026-11-27T12:00:00Z","type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_name":"Spark","primary":{"used_percent":3}}}}"#;
        let l = codex_rollout(line).limits.unwrap();
        assert_eq!(l.observed_at, at("2026-11-27T12:00:00Z"));
        assert!(l.parsed.buckets.is_empty());
        assert_eq!(l.parsed.unparsed, vec![r#"primary: {"used_percent":3}"#]);
        let first = line.lines().next().unwrap();
        let l = codex_rollout(first).limits.unwrap();
        assert_eq!(
            l.parsed.buckets,
            vec![
                b("all", "5h", 41.5, Some("20261203T121320Z")),
                b("all", "week", 12.0, None),
            ]
        );
        assert_eq!(l.limit_reached.as_deref(), Some("primary"));
        assert_eq!(codex_rollout("{}\nnot json\n"), Rollout::default());
    }

    #[test]
    fn codex_accounts_read_only_the_id() {
        assert_eq!(
            codex_account(&fixture("codex-auth.synthetic.json")).unwrap(),
            "acct-fixture-0001"
        );
        assert!(codex_account(r#"{"OPENAI_API_KEY":"sk-x","tokens":null}"#).is_err());
        assert!(codex_account("").is_err());
    }
}
