//! What a harness has seen of the conversations it follows: written by its followers (a transcript, a rollout, a
//! session record), read by `state` and `received`. A conversation is followed from the first time it is asked about
//! until nobody has asked for a while.

use super::harness::{Inbound, Model, State, Status};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a conversation nobody asks about is still followed: the connector asks every moment while it is joined.
const UNASKED: Duration = Duration::from_secs(30);
/// How many messages read are remembered per conversation.
const READ_KEPT: usize = 512;

/// One followed conversation. `extra` is the harness's own state for it.
pub struct Watch<S = ()> {
    pub extra: S,
    working: Mutex<Option<bool>>,
    engine: Mutex<Option<Value>>,
    inbound: Mutex<Inbound>,
    read: Mutex<VecDeque<String>>,
    asked: Mutex<Instant>,
    stopped: AtomicBool,
}

impl<S> Watch<S> {
    fn new(extra: S) -> Self {
        Self {
            extra,
            working: Mutex::new(None),
            engine: Mutex::new(None),
            inbound: Mutex::new(Inbound::Open),
            read: Mutex::new(VecDeque::new()),
            asked: Mutex::new(Instant::now()),
            stopped: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub fn default_for_test() -> Self
    where
        S: Default,
    {
        Self::new(S::default())
    }

    /// Whether following should end: nobody asked about the conversation for a while.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// Ends following now.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    pub fn set_working(&self, working: bool) {
        *self.working.lock().unwrap() = Some(working);
    }

    /// The engine it runs on, `{model, effort, thinking}`; one without a model is not one.
    pub fn set_engine(&self, engine: Value) {
        if Model::from_engine(&engine).is_some() {
            *self.engine.lock().unwrap() = Some(engine);
        }
    }

    /// The engine last set.
    #[cfg(test)]
    pub fn engine(&self) -> Option<Value> {
        self.engine.lock().unwrap().clone()
    }

    pub fn set_inbound(&self, inbound: Inbound) {
        *self.inbound.lock().unwrap() = inbound;
    }

    /// That the agent has the message `id`.
    pub fn read(&self, id: &str) {
        let mut read = self.read.lock().unwrap();
        if read.iter().any(|seen| seen == id) {
            return;
        }
        read.push_back(id.to_owned());
        while read.len() > READ_KEPT {
            read.pop_front();
        }
    }

    /// The state followed so far. Not working until a follower has seen otherwise.
    pub fn state(&self) -> State {
        State {
            status: if *self.working.lock().unwrap() == Some(true) {
                Status::Working
            } else {
                Status::Idle
            },
            model: self
                .engine
                .lock()
                .unwrap()
                .as_ref()
                .and_then(Model::from_engine),
            inbound: self.inbound.lock().unwrap().clone(),
        }
    }

    /// Which of `ids` the agent has.
    pub fn received(&self, ids: &[String]) -> Vec<String> {
        let read = self.read.lock().unwrap();
        ids.iter().filter(|id| read.contains(id)).cloned().collect()
    }
}

/// The conversations one harness follows, by id.
pub struct Watches<S = ()> {
    watches: Mutex<HashMap<String, Arc<Watch<S>>>>,
}

impl<S> Default for Watches<S> {
    fn default() -> Self {
        Self {
            watches: Mutex::new(HashMap::new()),
        }
    }
}

impl<S: Default> Watches<S> {
    /// The watch of conversation `id`, which `start` begins following the first time it is asked for. Watches nobody
    /// asked about for a while are stopped and forgotten.
    pub fn get(&self, id: &str, start: impl FnOnce(&Arc<Watch<S>>)) -> Arc<Watch<S>> {
        let mut watches = self.watches.lock().unwrap();
        watches.retain(|_, watch| {
            let kept = watch.asked.lock().unwrap().elapsed() < UNASKED;
            if !kept {
                watch.stop();
            }
            kept
        });
        if let Some(watch) = watches.get(id) {
            *watch.asked.lock().unwrap() = Instant::now();
            return watch.clone();
        }
        let watch = Arc::new(Watch::new(S::default()));
        watches.insert(id.to_owned(), watch.clone());
        drop(watches);
        start(&watch);
        watch
    }
}

/// How many times in a row a follower may fail to tell which file it has open before it gives up on the file.
const IDENTITY_ATTEMPTS: u32 = 10;

/// Counts a follower's consecutive failures to tell which file it has open ([`Fs::identity`](crate::platform::fs::Fs)):
/// a refusal that never goes away must not be retried in silence for ever.
#[derive(Default)]
pub struct IdentityFailures(u32);

impl IdentityFailures {
    /// The identity was read.
    pub fn read(&mut self) {
        self.0 = 0;
    }

    /// The identity could not be read. After [`IDENTITY_ATTEMPTS`] in a row the transcript is reported unreadable,
    /// in the connector's log (what `voice_diagnose` shows), and this returns `true`: the follower stops.
    pub fn failed(&mut self, error: &std::io::Error) -> bool {
        self.0 += 1;
        if self.0 < IDENTITY_ATTEMPTS {
            return false;
        }
        crate::logfile::log(&format!(
            "Transcript unreadable: cannot tell which file is open after {IDENTITY_ATTEMPTS} attempts ({error}); not following it"
        ));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_watch_is_started_once_and_tells_what_its_followers_saw() {
        let watches = Watches::<()>::default();
        let mut started = 0;
        let watch = watches.get("c", |_| started += 1);
        assert_eq!(watch.state().status, Status::Idle);
        watch.set_working(true);
        watch.set_engine(json!({"model":"m","effort":"high","thinking":null}));
        watch.set_engine(json!({"model":"","effort":"low"}));
        watch.read("m1");
        let again = watches.get("c", |_| started += 1);
        assert_eq!(started, 1);
        let state = again.state();
        assert_eq!(state.status, Status::Working);
        assert_eq!(state.model.unwrap().effort.as_deref(), Some("high"));
        assert_eq!(
            again.received(&["m1".into(), "m2".into()]),
            vec!["m1".to_owned()]
        );
    }
}
