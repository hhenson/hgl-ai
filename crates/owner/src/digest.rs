//! Console-generated Slack batches (OWN-1, OWN-9).

use hgl_ai_proto::SLACK_DIGEST_PREFIX;
use serde::{Deserialize, Serialize};

pub fn subject(team: &str, channel: &str) -> String {
    format!("{SLACK_DIGEST_PREFIX}.{team}.{channel}")
}

pub fn is_subject(subject: &str) -> bool {
    subject
        .strip_prefix(SLACK_DIGEST_PREFIX)
        .is_some_and(|tail| tail.starts_with('.'))
}

/// One persisted batch is also the checkpoint for the next round.
#[derive(Debug, Deserialize, Serialize)]
pub struct Digest {
    pub v: u32,
    /// Exclusive end of the report and question window, in Unix seconds.
    pub through: i64,
    /// Hash of the last decision log line read, even if the Slack preview clipped it.
    pub decision_tip: Option<String>,
    pub reports: Vec<Report>,
    pub decisions: Vec<String>,
    pub questions: Vec<Question>,
    pub more_reports: usize,
    pub more_decisions: usize,
    pub more_questions: usize,
}

impl Digest {
    pub fn is_empty(&self) -> bool {
        self.reports.is_empty() && self.decisions.is_empty() && self.questions.is_empty()
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Report {
    pub seq: u64,
    pub at: String,
    pub body: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Question {
    pub id: String,
    pub title: String,
}
