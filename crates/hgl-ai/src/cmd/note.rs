//! `hgl-ai note`: send, wait for and peek at notes (docs/protocol.md §2, §4).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, bail};
use hgl_ai_bridge::holder;
use hgl_ai_bridge::lock::{self, FileLock};
use hgl_ai_bus::{Bus, BusError, Delivery, Lease, MAX_NOTE_BYTES, StreamNote};
use hgl_ai_proto::{Kind, Note, Seat, TaskClass, seat_lock_key};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use super::{Ctx, EXIT_OK, NotReady};

/// How long `note peek` waits for a note.
const PEEK_WAIT: Duration = Duration::from_secs(1);
/// How often an interactive receive writes its seat lease again, well within the
/// `max_age` of `HGL_LOCKS` (docs/protocol.md §8).
const LEASE_REFRESH: Duration = Duration::from_secs(60);

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
    /// Print the notes SEAT's inbox has not acknowledged, without taking or acknowledging
    /// them (a peek spends no delivery).
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
    /// Task class for `assign` (simple, standard, complex; docs/roles.md §3), or the class
    /// a `sendback` moves the task up to (ROLE-15).
    #[arg(long, value_name = "CLASS")]
    class: Option<TaskClass>,
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
    /// Wait without the seat lease, even though a bridge serves the seat or holds it.
    #[arg(long)]
    force: bool,
}

#[derive(clap::Args)]
struct PeekArgs {
    seat: Seat,
    #[arg(long, value_name = "N", default_value_t = 10)]
    max: usize,
    /// Peek without the seat lease, even though a bridge serves the seat or holds it.
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
    if a.from.is_reserved() && !a.from.is_screen() {
        bail!(
            "--from {}: `owner` and `console` speak only through the console (docs/owner.md, OWN-5)",
            a.from
        );
    }
    if hgl_ai_proto::build::is_build_address(&a.from) {
        bail!(
            "--from {}: `builder-*` and `r-*` are builders' and builds' addresses, never a sender here",
            a.from
        );
    }
    let mut note = Note::new(a.from, a.to, a.kind, body);
    note.repo = a.repo;
    note.issue = a.issue.unwrap_or(0);
    note.head = a.head;
    note.base = a.base;
    note.class = a.class;
    note.validate()?;
    let cfg = ctx.load_config()?;
    hgl_ai_gh::routing::redirect(&cfg.github, &mut note)?;
    let bus = Bus::connect(&cfg).await?;
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

/// `wait` (acknowledge after printing) and `peek` (read the stream; nothing is taken or
/// acknowledged). Either holds the seat's lock on this host, `<data_dir>/seats/<seat>.lock`,
/// and the seat lease `HGL_LOCKS/seat.<seat>` from before the first read until it returns
/// (PROTO-6), so a bridge started meanwhile does not serve the seat; `--force` goes
/// without both.
async fn receive(
    ctx: &Ctx,
    seat: &Seat,
    max: usize,
    wait: Duration,
    force: bool,
    ack: bool,
) -> anyhow::Result<i32> {
    let cfg = ctx.load_config()?;
    let bus = Bus::connect(&cfg).await?;
    // Caught from here on, so an interrupted receive still gives the lease up.
    let mut sigs = Interrupts::install();
    let key = seat_lock_key(seat);
    if force {
        eprintln!(
            "warning: --force: receiving for seat {seat} without its lock and lease {key}; a \
             bridge may serve the seat at the same time"
        );
        return receive_held(ctx, &bus, seat, max, wait, ack, None, &mut sigs).await;
    }
    served_by_bridge(&bus, seat).await?;
    let path = cfg.seat_lock_path(seat)?;
    let _flock = match FileLock::try_exclusive(&path)
        .with_context(|| format!("the seat lock {}", path.display()))?
    {
        Some(l) => l,
        None => bail!(
            "seat {seat} is served on this host by pid {} (it holds {}); a seat is served by a \
             bridge or interactively, never both (--force to override)",
            lock::holder(&path).map_or_else(|| "unknown".to_string(), |p| p.to_string()),
            path.display()
        ),
    };
    let what = if ack { "note wait" } else { "note peek" };
    let id = holder::host_id(&cfg.data_dir()?);
    let me = holder::holder(&cfg.host, &id, what);
    let mut lease = SeatLease::take(&bus, seat, &me, &cfg.host, &id).await?;
    // Checked again under the lease: a bridge that advertised the seat meanwhile.
    let r = match served_by_bridge(&bus, seat).await {
        Ok(()) => receive_held(ctx, &bus, seat, max, wait, ack, Some(&mut lease), &mut sigs).await,
        Err(e) => Err(e),
    };
    lease.release().await;
    r
}

/// Refuse a seat that a live host record lists as served by its bridge.
async fn served_by_bridge(bus: &Bus, seat: &Seat) -> anyhow::Result<()> {
    if let Some(host) = bus.bridge_host(seat).await? {
        bail!(
            "seat {seat} is served by the bridge on host {host}; a seat is served by a bridge \
             or interactively, never both (--force to override)"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn receive_held(
    ctx: &Ctx,
    bus: &Bus,
    seat: &Seat,
    max: usize,
    wait: Duration,
    ack: bool,
    lease: Option<&mut SeatLease>,
    sigs: &mut Interrupts,
) -> anyhow::Result<i32> {
    if !ack {
        // A peek reads the stream after the inbox's acknowledged floor: no delivery is spent.
        let got = tokio::select! {
            r = bus.peek(seat, max.max(1), wait) => r?,
            _ = lost(lease) => bail!(
                "the seat lease {} is no longer ours (it expired, another holder took it, or the hub did not \
             confirm it in time); \
                 stopped",
                seat_lock_key(seat)
            ),
            sig = sigs.recv() => return Ok(128 + sig),
        };
        if got.is_empty() {
            return Err(NotReady(format!("no note for {seat}")).into());
        }
        let mut out = std::io::stdout().lock();
        for n in &got {
            print_peeked(&mut out, n, ctx.json, seat).context("printing a note")?;
        }
        return Ok(EXIT_OK);
    }
    let mut inbox = bus.inbox(seat).await?;
    let got = tokio::select! {
        r = inbox.fetch(max.max(1), wait) => r?,
        _ = lost(lease) => bail!(
            "the seat lease {} is no longer ours (it expired, another holder took it, or the hub did not \
             confirm it in time); \
             stopped waiting",
            seat_lock_key(seat)
        ),
        sig = sigs.recv() => return Ok(128 + sig),
    };
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
        d.ack().await?;
    }
    Ok(EXIT_OK)
}

/// The seat lease of an interactive receive: written again every [`LEASE_REFRESH`] in the
/// background, released by [`SeatLease::release`] (or, dropped, in the background).
struct SeatLease {
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    lost: watch::Receiver<bool>,
}

impl SeatLease {
    /// Take `seat.<seat>` as `me`; refused, naming the holder, when it is held. A holder of
    /// this host (label `host`, id `id`) whose process is gone, such as a `note wait` killed
    /// with SIGKILL, is taken over at once (PROTO-6): this receive holds the seat's lock here,
    /// so no live server of this host holds the lease.
    async fn take(
        bus: &Bus,
        seat: &Seat,
        me: &str,
        host: &str,
        id: &str,
    ) -> anyhow::Result<SeatLease> {
        let key = seat_lock_key(seat);
        let held = |holder: String, since: String| {
            anyhow::anyhow!(
                "seat {seat} is held by {holder} (lease {key} since {since}); a seat is served \
                 by a bridge or interactively, never both (--force to override)"
            )
        };
        let lease = match bus.lock_acquire(&key, me).await {
            Ok(l) => l,
            Err(BusError::Held { holder: h, .. }) if holder::dead_own(&h, host, id) => {
                eprintln!("taking over the seat lease {key} from {h}, which is gone");
                match bus.lock_take_over(&key, me, &h).await {
                    Ok(l) => l,
                    Err(BusError::Held { holder, since, .. }) => return Err(held(holder, since)),
                    Err(e) => return Err(e.into()),
                }
            }
            Err(BusError::Held { holder, since, .. }) => return Err(held(holder, since)),
            Err(e) => return Err(e.into()),
        };
        let (lost_tx, lost) = watch::channel(false);
        let (stop, stopped) = oneshot::channel();
        // As a bridge keeps its lease (PROTO-6): refreshed well within `max_age`, and given
        // up as lost when the hub has not confirmed it for half of it.
        let max_age = bus
            .locks_max_age()
            .await
            .unwrap_or(hgl_ai_bus::LOCKS_MAX_AGE);
        let every = LEASE_REFRESH
            .min(max_age / 6)
            .max(Duration::from_millis(50));
        let task = tokio::spawn(keep_lease(lease, every, max_age / 2, stopped, lost_tx));
        Ok(SeatLease {
            stop: Some(stop),
            task: Some(task),
            lost,
        })
    }

    /// Stop refreshing and delete the lease if it is still ours; returns once the hub answered.
    async fn release(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// Resolves once a refresh found the lease no longer ours; never without a lease.
async fn lost(lease: Option<&mut SeatLease>) {
    if let Some(l) = lease
        && l.lost.wait_for(|lost| *lost).await.is_ok()
    {
        return;
    }
    std::future::pending().await
}

async fn keep_lease(
    mut lease: Lease,
    every: Duration,
    deadline: Duration,
    mut stopped: oneshot::Receiver<()>,
    lost: watch::Sender<bool>,
) {
    let mut ok_at = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = &mut stopped => break,
            _ = tokio::time::sleep(every) => {
                let failed = match tokio::time::timeout(every, lease.refresh()).await {
                    Ok(Ok(())) => {
                        ok_at = tokio::time::Instant::now();
                        continue;
                    }
                    Ok(Err(BusError::LeaseLost { .. })) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(_) => Some(format!("no answer within {every:?}")),
                };
                if let Some(e) = &failed
                    && ok_at.elapsed() < deadline
                {
                    eprintln!("warning: refreshing the seat lease {}: {e}; trying again", lease.name());
                    continue;
                }
                lost.send_replace(true);
                // Keep the sender until told to stop, so the loss stays visible.
                let _ = stopped.await;
                return;
            }
        }
    }
    let name = lease.name().to_string();
    match lease.release().await {
        Ok(true) => {}
        Ok(false) => eprintln!("warning: the seat lease {name} was no longer ours at release"),
        Err(e) => eprintln!("warning: releasing the seat lease {name}: {e}; it expires on its own"),
    }
}

/// SIGINT, SIGTERM and SIGHUP, caught while a receive runs.
struct Interrupts([(i32, Option<Signal>); 3]);

impl Interrupts {
    fn install() -> Interrupts {
        let on = |k| signal(k).ok();
        // The numbers are the same on Linux and macOS.
        Interrupts([
            (2, on(SignalKind::interrupt())),
            (15, on(SignalKind::terminate())),
            (1, on(SignalKind::hangup())),
        ])
    }

    /// Resolves with the number of the first of them received.
    async fn recv(&mut self) -> i32 {
        async fn one(n: i32, s: &mut Option<Signal>) -> i32 {
            if let Some(s) = s
                && s.recv().await.is_some()
            {
                return n;
            }
            std::future::pending().await
        }
        let [(a, sa), (b, sb), (c, sc)] = &mut self.0;
        tokio::select! {
            n = one(*a, sa) => n,
            n = one(*b, sb) => n,
            n = one(*c, sc) => n,
        }
    }
}

/// A peeked note, printed as `wait` prints a delivery (no `delivered=`: none was spent).
fn print_peeked(
    out: &mut impl Write,
    n: &StreamNote,
    json: bool,
    seat: &Seat,
) -> anyhow::Result<()> {
    print_delivery(
        out,
        &Delivery::detached(n.seq, 1, n.note.clone()),
        json,
        seat,
    )
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
/// broadcast, `base=` and `class=` when set and `delivered=N` on a redelivery.
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
    if let Some(x) = n.class {
        h.push_str(&format!(" class={x}"));
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
        let mut a = Note::new(seat("triage"), seat("w1"), Kind::Assign, "[claude] Assign");
        a.created_at = "20261127T041100Z".into();
        a.repo = Some("hgl-ai".into());
        a.issue = 17;
        a.class = Some(TaskClass::Simple);
        let d = Delivery::detached(14, 1, a);
        assert_eq!(
            header(&d, &seat("w1")),
            "--- seq=14 from=triage kind=assign hgl-ai#17 class=simple at 20261127T041100Z"
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
