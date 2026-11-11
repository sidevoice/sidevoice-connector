//! The per-id record of what was handed to each conversation's harness, kept across a restart in `delivered.json`:
//! each message id as `accepted` (handed over: the agent's inbox took it, or a write may have reached it) or `received`
//! (the agent has it in the conversation). It is what makes the core's at-least-once delivery safe: a message offered
//! again is answered from here and never typed into the agent a second time, and the core's question about which
//! messages the agent has (`input.received`) is answered from here too.
//!
//! Bounded: [`IDS_PER_THREAD`] ids per conversation (the oldest go first) and [`THREADS`] conversations (the one used
//! least recently goes first). A conversation's record is dropped when it ends.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};

/// Most message ids remembered per conversation.
pub const IDS_PER_THREAD: usize = 256;
/// Most conversations remembered.
pub const THREADS: usize = 1024;
/// The longest message id the record keeps (the core's are far shorter). A longer one is refused before it is handed
/// over: a message that cannot be remembered could be typed in again on every offer.
pub const ID_MAX: usize = 128;

#[derive(Default)]
struct Thread {
    /// When it was last written to, on the record's own clock.
    used: u64,
    ids: VecDeque<(String, String)>,
}

#[derive(Default)]
pub struct Record {
    threads: HashMap<String, Thread>,
    clock: u64,
}

impl Record {
    /// The record as [`Record::to_json`] wrote it: `{thread: {used, ids: [[id, status], …]}}`. Only what the connector
    /// itself writes is believed (a bounded id, `accepted` or `received`); anything else is left out, and a record that
    /// does not parse is a record lost (a message may then be handed over once more), never a connector that cannot
    /// start.
    pub fn from_json(value: &Value) -> Self {
        let mut record = Self::default();
        for (thread, saved) in value.as_object().into_iter().flatten() {
            let ids: VecDeque<(String, String)> = saved
                .get("ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|row| {
                    let id = row
                        .get(0)?
                        .as_str()
                        .filter(|id| !id.is_empty() && id.len() <= ID_MAX)?;
                    let status = row
                        .get(1)?
                        .as_str()
                        .filter(|status| matches!(*status, "accepted" | "received"))?;
                    Some((id.to_owned(), status.to_owned()))
                })
                .collect();
            let used = saved.get("used").and_then(Value::as_u64).unwrap_or(0);
            record.clock = record.clock.max(used);
            let mut thread_record = Thread { used, ids };
            while thread_record.ids.len() > IDS_PER_THREAD {
                thread_record.ids.pop_front();
            }
            record.threads.insert(thread.clone(), thread_record);
        }
        while record.threads.len() > THREADS {
            record.evict();
        }
        record
    }

    pub fn to_json(&self) -> Value {
        Value::Object(
            self.threads
                .iter()
                .map(|(thread, saved)| (thread.clone(), json!({"used":saved.used,"ids":saved.ids})))
                .collect(),
        )
    }

    /// What it says of `id` in `thread`: `accepted` or `received`.
    pub fn status(&self, thread: &str, id: &str) -> Option<&str> {
        self.threads
            .get(thread)?
            .ids
            .iter()
            .find(|(seen, _)| seen == id)
            .map(|(_, status)| status.as_str())
    }

    /// The ids handed to `thread` that the agent has not been seen to have, oldest first.
    pub fn awaiting(&self, thread: &str) -> Vec<String> {
        self.threads
            .get(thread)
            .into_iter()
            .flat_map(|saved| saved.ids.iter())
            .filter(|(_, status)| status == "accepted")
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Records `id` in `thread` as `status`; a message received stays received. Whether anything changed.
    pub fn set(&mut self, thread: &str, id: &str, status: &str) -> bool {
        if id.len() > ID_MAX {
            return false;
        }
        if !self.threads.contains_key(thread) && self.threads.len() >= THREADS {
            self.evict();
        }
        self.clock += 1;
        let clock = self.clock;
        let saved = self.threads.entry(thread.to_owned()).or_default();
        saved.used = clock;
        if let Some(row) = saved.ids.iter_mut().find(|(seen, _)| seen == id) {
            if row.1 == "received" || row.1 == status {
                return false;
            }
            row.1 = status.to_owned();
            return true;
        }
        saved.ids.push_back((id.to_owned(), status.to_owned()));
        while saved.ids.len() > IDS_PER_THREAD {
            saved.ids.pop_front();
        }
        true
    }

    /// Drops `thread`'s record: its conversation ended. Whether there was one.
    pub fn forget(&mut self, thread: &str) -> bool {
        self.threads.remove(thread).is_some()
    }

    /// Drops the conversation written to least recently.
    fn evict(&mut self) {
        if let Some(oldest) = self
            .threads
            .iter()
            .min_by_key(|(_, saved)| saved.used)
            .map(|(thread, _)| thread.clone())
        {
            self.threads.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_conversation_used_least_recently_goes_first_and_survives_a_write_and_read() {
        let mut record = Record::default();
        for n in 0..THREADS {
            assert!(record.set(&format!("t{n}"), "m", "accepted"));
        }
        // The first conversation is used again: the second is now the least recent.
        assert!(record.set("t0", "m2", "accepted"));
        let record = Record::from_json(&record.to_json());
        let mut record = record;
        assert!(record.set("new", "m", "accepted"));
        assert_eq!(record.status("t0", "m2"), Some("accepted"));
        assert_eq!(
            record.status("t1", "m"),
            None,
            "least recently used, evicted"
        );
        assert_eq!(
            record.status(&format!("t{}", THREADS - 1), "m"),
            Some("accepted")
        );
    }

    #[test]
    fn received_stays_received_and_an_overlong_id_is_never_kept() {
        let mut record = Record::default();
        assert!(record.set("t", "m", "accepted"));
        assert_eq!(record.awaiting("t"), ["m"]);
        assert!(record.set("t", "m", "received"));
        assert!(!record.set("t", "m", "accepted"));
        assert_eq!(record.status("t", "m"), Some("received"));
        assert!(record.awaiting("t").is_empty());
        assert!(!record.set("t", &"x".repeat(ID_MAX + 1), "accepted"));
        assert!(record.forget("t"));
        assert_eq!(record.status("t", "m"), None);
    }

    #[test]
    fn only_what_the_connector_writes_is_believed() {
        let record = Record::from_json(
            &json!({"t":{"used":3,"ids":[["m-1","failed"],["m-2","accepted"],["","accepted"],["m-3","received"]]},
            "old-format":[["m-4","accepted"]]}),
        );
        assert_eq!(record.status("t", "m-1"), None);
        assert_eq!(record.status("t", "m-2"), Some("accepted"));
        assert_eq!(record.status("t", "m-3"), Some("received"));
        assert_eq!(record.status("old-format", "m-4"), None);
    }
}
