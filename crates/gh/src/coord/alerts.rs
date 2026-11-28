use hgl_ai_bus::{Bus, BusError, MAX_VALUE_BYTES};
use hgl_ai_proto::{Kind, Note, check_head};
use serde::{Deserialize, Serialize};

use super::{Alert, KEY_UNSENT, alert_note};
use crate::{Result, Settings};

const UNSENT_KEEP: usize = 100;
const SEND_ATTEMPTS: u8 = 5;
// Leave room below the hub's value limit for the JetStream write headers.
const QUEUE_BYTES: usize = MAX_VALUE_BYTES - 1024;
// Space for the fixed-size overflow report, including its envelope.
const REPORT_BYTES: usize = 1024;
const ALERT_BYTES: usize = QUEUE_BYTES - REPORT_BYTES;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Finding {
    Review,
    Check,
    ReviewAndCheck,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Entry {
    #[serde(flatten)]
    note: Note,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    finding: Option<Finding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compared_revision: Option<u64>,
    #[serde(default)]
    attempts: u8,
    #[serde(default)]
    report: bool,
}

impl Entry {
    fn alert(s: &Settings, a: &Alert, compared_revision: u64) -> Self {
        let finding = match (a.finding, a.failing) {
            (true, true) => Finding::ReviewAndCheck,
            (true, false) => Finding::Review,
            _ => Finding::Check,
        };
        Self {
            note: alert_note(s, a),
            finding: Some(finding),
            compared_revision: Some(compared_revision),
            attempts: 0,
            report: false,
        }
    }

    fn finding(&self) -> Option<Finding> {
        // Older coord.alerts entries were just Notes. Their first line names the finding.
        self.finding.or_else(|| {
            let line = self
                .note
                .body
                .lines()
                .next()?
                .strip_prefix("[coord] Alert: ")?;
            let (_, suffix) = line.rsplit_once(") has ")?;
            match suffix {
                "a review bot finding at this head." => Some(Finding::Review),
                "a failing check at this head." => Some(Finding::Check),
                "a review bot finding and a failing check at this head." => {
                    Some(Finding::ReviewAndCheck)
                }
                _ => None,
            }
        })
    }

    fn report(s: &Settings, body: String) -> Self {
        Self {
            note: Note::new(s.coord_seat.clone(), s.controller.clone(), Kind::Note, body),
            finding: None,
            compared_revision: None,
            attempts: 0,
            report: true,
        }
    }

    fn message_id(&self) -> String {
        let Some(revision) = self.compared_revision else {
            return self.note.msg_id();
        };
        // A hold or failed GitHub read can drain the queue while the comparison
        // table is unchanged. A later observation of that transition reuses this ID.
        let key = (
            revision,
            &self.note.repo,
            self.note.issue,
            &self.note.head,
            self.finding,
        );
        format!(
            "coord-alert/{}",
            serde_json::to_string(&key).expect("alert identity encodes")
        )
    }

    fn dead_letter(&self, s: &Settings, reason: &str) -> Self {
        let excerpt: String = self.note.body.chars().take(256).collect();
        let reason: String = reason.chars().take(256).collect();
        let mut report = Self::report(
            s,
            format!(
                "[coord] Dead letter: coordinator alert after {} failed sends\n{reason}\n{excerpt}",
                self.attempts
            ),
        );
        report.note.repo = self.note.repo.clone().filter(|r| r.len() <= 100);
        report.note.issue = self.note.issue;
        report.note.head = self.note.head.clone().filter(|h| check_head(h).is_ok());
        report.finding = self.finding();
        tracing::warn!(
            repo = ?report.note.repo,
            issue = report.note.issue,
            head = ?report.note.head,
            "dead-lettering coordinator alert: {reason}"
        );
        report
    }
}

fn append(entries: &mut Vec<Entry>, new: impl IntoIterator<Item = Entry>) {
    for entry in new {
        let finding = entry.finding();
        // Keep the original envelope and retry count, and let reports coexist with alerts.
        let exists = entries.iter().any(|queued| {
            queued.report == entry.report
                && ((finding.is_some()
                    && queued.note.repo == entry.note.repo
                    && queued.note.issue == entry.note.issue
                    && queued.note.head == entry.note.head
                    && queued.finding() == finding)
                    || queued.note == entry.note)
        });
        if !exists {
            entries.push(entry);
        }
    }
}

/// Bound both the in-memory staging buffer and the durable value. Count UTF-8 and JSON
/// escaping as encoded by serde, plus brackets and a comma allowance for each entry.
fn bound(s: &Settings, entries: &mut Vec<Entry>) {
    let sizes: Vec<_> = entries
        .iter_mut()
        .map(|entry| {
            let mut size = serde_json::to_vec(entry).expect("alert encodes").len() + 1;
            if size + 2 > ALERT_BYTES {
                *entry = entry.dead_letter(s, "the encoded alert exceeds the queue byte limit");
                size = serde_json::to_vec(entry)
                    .expect("dead letter encodes")
                    .len()
                    + 1;
            }
            size
        })
        .collect();
    let mut bytes = 2 + sizes.iter().sum::<usize>();
    if entries.len() <= UNSENT_KEEP && bytes <= QUEUE_BYTES {
        return;
    }
    let mut dropped = 0;
    while entries.len() - dropped >= UNSENT_KEEP || bytes > ALERT_BYTES {
        bytes -= sizes[dropped];
        dropped += 1;
    }
    entries.drain(..dropped);
    tracing::warn!(
        dropped,
        "coordinator alert queue full; dropped oldest entries"
    );
    entries.push(Entry::report(
        s,
        format!("[coord] Alert queue full: dropped {dropped} oldest entries"),
    ));
}

pub(super) struct Queue {
    entries: Vec<Entry>,
    revision: u64,
}

impl Queue {
    pub(super) async fn load(b: &Bus) -> Result<Self> {
        let Some(saved) = b.state_get(KEY_UNSENT).await? else {
            return Ok(Self {
                entries: Vec::new(),
                revision: 0,
            });
        };
        let entries = serde_json::from_str(&saved.value).unwrap_or_else(|e| {
            tracing::warn!("{KEY_UNSENT} is not readable ({e}); dropped");
            Vec::new()
        });
        Ok(Self {
            entries,
            revision: saved.revision,
        })
    }

    async fn save(&mut self, b: &Bus, s: &Settings) -> Result<()> {
        bound(s, &mut self.entries);
        let text = if self.entries.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&self.entries).expect("alerts encode")
        };
        self.revision = b.state_update(KEY_UNSENT, &text, self.revision).await?;
        Ok(())
    }

    pub(super) async fn drain(mut self, b: &Bus, s: &Settings) -> Result<()> {
        let mut failed = None;
        let mut changed = self.entries.is_empty() && self.revision != 0;
        for mut entry in std::mem::take(&mut self.entries) {
            match b.send_with_id(&entry.note, &entry.message_id()).await {
                Ok(_) => {
                    changed = true;
                    tracing::info!(
                        "alert sent: {}",
                        entry.note.body.lines().next().unwrap_or("")
                    );
                }
                Err(e) if entry.report => {
                    if matches!(e, BusError::Proto(_)) {
                        tracing::warn!("dropping invalid coordinator alert report: {e}");
                        changed = true;
                    } else {
                        tracing::warn!("retrying coordinator alert report: {e}");
                        self.entries.push(entry);
                    }
                    failed.get_or_insert(e);
                }
                Err(e) => {
                    changed = true;
                    entry.attempts = entry.attempts.saturating_add(1);
                    if entry.attempts >= SEND_ATTEMPTS {
                        entry = entry.dead_letter(s, &e.to_string());
                    }
                    self.entries.push(entry);
                    failed.get_or_insert(e);
                }
            }
        }
        if changed {
            self.save(b, s).await?;
        }
        failed.map_or(Ok(()), |e| Err(e.into()))
    }
}

/// Save new entries alongside outstanding ones before the caller commits the new PR table.
pub(super) async fn stage(
    b: &Bus,
    s: &Settings,
    pending: &mut Vec<Entry>,
    new: &[Alert],
    compared_revision: u64,
) -> Result<Queue> {
    append(
        pending,
        new.iter().map(|a| Entry::alert(s, a, compared_revision)),
    );
    bound(s, pending);
    let mut queue = Queue::load(b).await?;
    let before = queue.entries.len();
    append(&mut queue.entries, pending.iter().cloned());
    if queue.entries.len() != before || (queue.entries.is_empty() && queue.revision != 0) {
        queue.save(b, s).await?;
    }
    pending.clear();
    Ok(queue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::NoteState;
    use crate::testutil::settings;

    fn alert() -> Alert {
        Alert {
            repo: "alpha".into(),
            number: 11,
            head: "1".repeat(40),
            note: NoteState::Ready,
            finding: true,
            failing: false,
            url: "https://github.com/o/alpha/pull/11".into(),
        }
    }

    #[test]
    fn legacy_ready_and_typed_approved_notes_share_the_same_finding() {
        let s = settings();
        let mut alert = alert();
        let original = alert_note(&s, &alert);
        let legacy = serde_json::to_string(&[&original]).unwrap();
        let mut queue: Vec<Entry> = serde_json::from_str(&legacy).unwrap();
        let ready = Entry::alert(&s, &alert, 1);
        alert.note = NoteState::Approved;
        let approved = Entry::alert(&s, &alert, 1);
        assert_eq!(ready.message_id(), approved.message_id());
        append(&mut queue, [approved]);
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].note, original);

        alert.finding = false;
        alert.failing = true;
        let check = Entry::alert(&s, &alert, 1);
        assert_ne!(ready.message_id(), check.message_id());
        append(&mut queue, [check]);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn a_queued_dead_letter_does_not_suppress_a_new_alert() {
        let s = settings();
        let alert = alert();
        let report = Entry::alert(&s, &alert, 1).dead_letter(&s, "previous send failed");
        let new = Entry::alert(&s, &alert, 2);
        let mut queue = vec![report.clone()];

        append(&mut queue, [new.clone(), report, new.clone()]);

        let [report, queued] = queue.as_slice() else {
            panic!("expected one report and one alert");
        };
        assert!(report.report);
        assert!(!queued.report);
        assert_eq!(queued.message_id(), new.message_id());
    }

    #[test]
    fn an_alert_that_cannot_fit_leaves_a_small_report() {
        let s = settings();
        let mut alert = alert();
        alert.url = "a".repeat(MAX_VALUE_BYTES);
        let mut queue = vec![Entry::alert(&s, &alert, 1)];
        bound(&s, &mut queue);
        assert!(queue[0].report);
        assert_eq!(queue[0].note.issue, alert.number);
        assert!(serde_json::to_vec(&queue).unwrap().len() < 1024);
    }

    #[test]
    fn an_alert_that_cannot_fit_with_the_overflow_report_keeps_older_entries() {
        let s = settings();
        let mut older = alert();
        older.url.push_str(&"x".repeat(REPORT_BYTES));
        let older = Entry::alert(&s, &older, 1);
        let mut large = alert();
        large.number += 1;
        let initial_size = serde_json::to_vec(&Entry::alert(&s, &large, 1))
            .unwrap()
            .len()
            + 3;
        large
            .url
            .push_str(&"x".repeat(ALERT_BYTES + 1 - initial_size));
        let mut queue = vec![older.clone(), Entry::alert(&s, &large, 1)];

        bound(&s, &mut queue);

        let [kept, report] = queue.as_slice() else {
            panic!("expected the older alert and a dead-letter report");
        };
        assert_eq!(kept.note, older.note);
        assert!(report.report);
        assert_eq!(report.note.issue, large.number);
    }
}
