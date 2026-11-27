//! `hgl-ai note`: send, wait for and peek at notes (docs/protocol.md §2, §4).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use hgl_ai_bus::{Bus, Delivery, MAX_NOTE_BYTES};
use hgl_ai_proto::{Kind, Note, Seat};

use super::{Ctx, EXIT_OK, NotReady};

/// How long `note peek` waits for a note.
const PEEK_WAIT: Duration = Duration::from_secs(1);

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Send a note; prints `seq N` (`seq N duplicate` when the hub dropped a resend).
    Send(SendArgs),
    /// Block until notes arrive for SEAT, print them and acknowledge them.
    /// Exit status 3 when --timeout passes with no note.
    Wait(WaitArgs),
    /// Print pending notes for SEAT and return them unacknowledged
    /// (each peek counts as a delivery toward the inbox's limit of 5).
    Peek(PeekArgs),
}

#[derive(clap::Args)]
#[command(group = clap::ArgGroup::new("body_source").required(true).args(["body", "body_file"]))]
struct SendArgs {
    /// Recipient seat, or `all`.
    #[arg(long, value_name = "SEAT")]
    to: Seat,
    /// Sending seat.
    #[arg(long = "from", value_name = "SEAT", env = "HGL_AI_SEAT")]
    from: Seat,
    /// assign, ready, blocked, question, answer, review, receipt, sendback,
    /// approved, landed, closed, done or note.
    #[arg(long)]
    kind: Kind,
    /// Repository name, such as hgl-compiler.
    #[arg(long, value_name = "REPO")]
    repo: Option<String>,
    /// Issue or pull request number.
    #[arg(long, value_name = "N")]
    issue: Option<u64>,
    /// Full 40-character commit the note is about.
    #[arg(long, value_name = "SHA")]
    head: Option<String>,
    /// Base for `assign`; merge commit for `landed`.
    #[arg(long, value_name = "B")]
    base: Option<String>,
    /// Markdown body.
    #[arg(long, value_name = "TEXT")]
    body: Option<String>,
    /// Read the body from FILE, or `-` for standard input.
    #[arg(long, value_name = "FILE")]
    body_file: Option<PathBuf>,
}

#[derive(clap::Args)]
struct WaitArgs {
    seat: Seat,
    /// Give up after this long (30s, 5m, 2h); default: wait without end.
    #[arg(long, value_name = "DUR", value_parser = duration)]
    timeout: Option<Duration>,
    /// Most notes to print at once.
    #[arg(long, value_name = "N", default_value_t = 10)]
    max: usize,
    /// Wait even though a bridge serves the seat.
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args)]
struct PeekArgs {
    seat: Seat,
    #[arg(long, value_name = "N", default_value_t = 10)]
    max: usize,
    /// Peek even though a bridge serves the seat.
    #[arg(long)]
    force: bool,
}

pub(super) fn duration(s: &str) -> Result<Duration, String> {
    hgl_ai_config::parse_duration(s)
        .ok_or_else(|| format!("invalid duration {s:?}: use 30s, 5m, 2h or 3d"))
}

/// Read at most `limit` bytes of UTF-8 text; more is an error, never buffered.
pub(super) fn read_bounded(r: impl Read, what: &str, limit: usize) -> anyhow::Result<String> {
    let mut buf = Vec::new();
    r.take(limit as u64 + 1)
        .read_to_end(&mut buf)
        .with_context(|| format!("reading {what}"))?;
    if buf.len() > limit {
        bail!("{what} is over the {limit} byte limit");
    }
    String::from_utf8(buf).with_context(|| format!("{what} is not UTF-8"))
}

pub async fn run(ctx: &Ctx, args: Args) -> anyhow::Result<i32> {
    match args.cmd {
        Cmd::Send(a) => send(ctx, a).await,
        Cmd::Wait(a) => {
            let wait = a.timeout.unwrap_or(Duration::MAX);
            receive(ctx, &a.seat, a.max, wait, a.force, true).await
        }
        Cmd::Peek(a) => receive(ctx, &a.seat, a.max, PEEK_WAIT, a.force, false).await,
    }
}

async fn send(ctx: &Ctx, a: SendArgs) -> anyhow::Result<i32> {
    let body = match (a.body, a.body_file) {
        (Some(b), _) => b,
        (None, Some(p)) if p.as_os_str() == "-" => {
            read_bounded(std::io::stdin().lock(), "standard input", MAX_NOTE_BYTES)?
        }
        (None, Some(p)) => {
            let f = std::fs::File::open(&p).with_context(|| format!("opening {}", p.display()))?;
            read_bounded(f, &p.display().to_string(), MAX_NOTE_BYTES)?
        }
        (None, None) => unreachable!("clap requires a body"),
    };
    let mut note = Note::new(a.from, a.to, a.kind, body);
    note.repo = a.repo;
    note.issue = a.issue.unwrap_or(0);
    note.head = a.head;
    note.base = a.base;
    note.validate()?;
    let bus = Bus::connect(&ctx.load_config()?).await?;
    let r = bus.send(&note).await?;
    if ctx.json {
        println!(
            "{}",
            serde_json::json!({ "seq": r.seq, "duplicate": r.duplicate })
        );
    } else if r.duplicate {
        println!("seq {} duplicate", r.seq);
    } else {
        println!("seq {}", r.seq);
    }
    Ok(EXIT_OK)
}

/// `wait` (acknowledge after printing) and `peek` (hand back at once, unacknowledged).
async fn receive(
    ctx: &Ctx,
    seat: &Seat,
    max: usize,
    wait: Duration,
    force: bool,
    ack: bool,
) -> anyhow::Result<i32> {
    let bus = Bus::connect(&ctx.load_config()?).await?;
    if !force && let Some(host) = bus.bridge_host(seat).await? {
        bail!(
            "seat {seat} is served by the bridge on host {host}; a seat is served by a bridge or \
             interactively, never both (--force to override)"
        );
    }
    let mut inbox = bus.inbox(seat).await?;
    let got = inbox.fetch(max.max(1), wait).await?;
    if got.is_empty() {
        let within = if wait == Duration::MAX {
            String::new()
        } else {
            format!(" within {}", hgl_ai_cap::format_duration(wait))
        };
        return Err(NotReady(format!("no note for {seat}{within}")).into());
    }
    let mut out = std::io::stdout().lock();
    for (i, d) in got.iter().enumerate() {
        if let Err(e) = print_delivery(&mut out, d, ctx.json, seat) {
            // Not shown, so hand back this note and the rest at once.
            for rest in &got[i..] {
                let _ = rest.nak(Duration::ZERO).await;
            }
            return Err(e.context("printing a note"));
        }
        if ack {
            d.ack().await?;
        } else {
            d.nak(Duration::ZERO).await?;
        }
    }
    Ok(EXIT_OK)
}

fn print_delivery(
    out: &mut impl Write,
    d: &Delivery,
    json: bool,
    seat: &Seat,
) -> anyhow::Result<()> {
    if json {
        let mut v = serde_json::to_value(&d.note)?;
        v["seq"] = d.seq.into();
        v["delivered"] = d.delivered.into();
        writeln!(out, "{v}")?;
    } else {
        writeln!(out, "{}", header(d, seat))?;
        out.write_all(d.note.body.as_bytes())?;
        if !d.note.body.ends_with('\n') {
            writeln!(out)?;
        }
    }
    out.flush()?;
    Ok(())
}

/// `--- seq=N from=S kind=K repo#issue head=SHA at STAMP`, with `to=all` for a
/// broadcast, `base=` when set and `delivered=N` on a redelivery.
fn header(d: &Delivery, seat: &Seat) -> String {
    let n = &d.note;
    let mut h = format!("--- seq={} from={}", d.seq, n.sender);
    if &n.recipient != seat {
        h.push_str(&format!(" to={}", n.recipient));
    }
    h.push_str(&format!(" kind={}", n.kind));
    match (&n.repo, n.issue) {
        (Some(r), 0) => h.push_str(&format!(" {r}")),
        (Some(r), i) => h.push_str(&format!(" {r}#{i}")),
        (None, 0) => {}
        (None, i) => h.push_str(&format!(" #{i}")),
    }
    if let Some(x) = &n.head {
        h.push_str(&format!(" head={x}"));
    }
    if let Some(x) = &n.base {
        h.push_str(&format!(" base={x}"));
    }
    if d.delivered > 1 {
        h.push_str(&format!(" delivered={}", d.delivered));
    }
    h.push_str(&format!(" at {}", n.created_at));
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(s: &str) -> Seat {
        Seat::new(s).unwrap()
    }

    #[test]
    fn headers() {
        let mut n = Note::new(seat("w4"), seat("claude"), Kind::Ready, "[codex] Ready");
        n.created_at = "20261127T041100Z".into();
        n.repo = Some("hgl-compiler".into());
        n.issue = 980;
        n.head = Some("8a5c3d264c2edbbdaa61530d810bff276c3eebba".into());
        let d = Delivery::detached(12, 1, n.clone());
        assert_eq!(
            header(&d, &seat("claude")),
            "--- seq=12 from=w4 kind=ready hgl-compiler#980 head=8a5c3d264c2edbbdaa61530d810bff276c3eebba at 20261127T041100Z"
        );
        let mut b = Note::new(seat("claude"), Seat::broadcast(), Kind::Note, "drain");
        b.created_at = "20261127T041100Z".into();
        let d = Delivery::detached(13, 2, b);
        assert_eq!(
            header(&d, &seat("w1")),
            "--- seq=13 from=claude to=all kind=note delivered=2 at 20261127T041100Z"
        );
    }

    #[test]
    fn printing() {
        let mut n = Note::new(seat("w4"), seat("claude"), Kind::Note, "line one\nline two");
        n.created_at = "20261127T041100Z".into();
        let d = Delivery::detached(5, 1, n);
        let mut text = Vec::new();
        print_delivery(&mut text, &d, false, &seat("claude")).unwrap();
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "--- seq=5 from=w4 kind=note at 20261127T041100Z\nline one\nline two\n"
        );
        let mut json = Vec::new();
        print_delivery(&mut json, &d, true, &seat("claude")).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            (
                v["seq"].as_u64(),
                v["delivered"].as_u64(),
                v["sender"].as_str()
            ),
            (Some(5), Some(1), Some("w4"))
        );
    }

    #[test]
    fn bounded_reads() {
        assert_eq!(read_bounded(&b"abc"[..], "x", 3).unwrap(), "abc");
        assert!(
            read_bounded(&b"abcd"[..], "x", 3)
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
        assert!(read_bounded(&[0xff][..], "x", 3).is_err());
        assert!(duration("5m").is_ok());
        assert!(duration("5x").is_err());
    }
}
