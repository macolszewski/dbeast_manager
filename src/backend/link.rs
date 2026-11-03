//! Whether each data source currently has a working connection, reported to
//! the explorer as [`Event::SourceLink`] only when it changes.
//!
//! Every session records whether its connection is open or lost. A source is
//! lost while any of its sessions is lost, and open while at least one is open.

use super::Event;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(super) type Send = Arc<dyn Fn(Event) + std::marker::Send + Sync>;

#[derive(Default)]
pub(super) struct Links {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next_generation: u64,
    /// Registered sessions: their generation and connection state.
    sessions: HashMap<(String, String), (u64, Option<bool>)>,
    /// The last state sent to the UI per data source.
    reported: HashMap<String, bool>,
}

/// One session's handle for reporting its connection state.
pub(super) struct SessionLink {
    links: Arc<Links>,
    key: (String, String),
    generation: u64,
    send: Send,
}

impl SessionLink {
    /// Writes to the Output log of the console that owns this session.
    pub(super) fn notice(&self, text: &str) {
        if let Ok(console_id) = self.key.1.parse() {
            (self.send)(Event::Notice {
                console_id,
                text: text.to_owned(),
            });
        }
    }

    /// `Some(true)` after connecting, `Some(false)` when the connection was
    /// lost, `None` when the session closed it on purpose.
    pub(super) fn set(&self, open: Option<bool>) {
        let changed = {
            let mut inner = self.links.inner.lock().unwrap();
            match inner.sessions.get_mut(&self.key) {
                // A session dropped by Disconnect must not bring the source back.
                Some((generation, state)) if *generation == self.generation => *state = open,
                _ => return,
            }
            inner.update(&self.key.0)
        };
        if let Some(open) = changed {
            (self.send)(Event::SourceLink {
                ds_id: self.key.0.clone(),
                open,
            });
        }
    }
}

impl Links {
    pub(super) fn register(self: &Arc<Self>, ds_id: &str, name: &str, send: Send) -> SessionLink {
        let mut inner = self.inner.lock().unwrap();
        inner.next_generation += 1;
        let generation = inner.next_generation;
        let key = (ds_id.to_owned(), name.to_owned());
        inner.sessions.insert(key.clone(), (generation, None));
        SessionLink {
            links: self.clone(),
            key,
            generation,
            send,
        }
    }

    pub(super) fn forget_source(&self, ds_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.sessions.retain(|(id, _), _| id != ds_id);
        inner.reported.remove(ds_id);
    }

    /// A closed console does not decide whether its source is lost.
    pub(super) fn forget_session(&self, ds_id: &str, name: &str) -> Option<bool> {
        let mut inner = self.inner.lock().unwrap();
        inner.sessions.remove(&(ds_id.to_owned(), name.to_owned()));
        inner.update(ds_id)
    }
}

impl Inner {
    /// Recomputes the source's state and returns it when it differs from the
    /// last one reported.
    fn update(&mut self, ds_id: &str) -> Option<bool> {
        let states = self
            .sessions
            .iter()
            .filter(|((id, _), _)| id == ds_id)
            .filter_map(|(_, (_, state))| *state);
        let mut current = None;
        for open in states {
            current = Some(open);
            if !open {
                break;
            }
        }
        // No session holds a connection: nothing new to report.
        let current = current?;
        (self.reported.insert(ds_id.to_owned(), current) != Some(current)).then_some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Reported = Arc<Mutex<Vec<(String, bool)>>>;

    fn collector() -> (Send, Reported) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let send: Send = Arc::new(move |event| {
            if let Event::SourceLink { ds_id, open } = event {
                sink.lock().unwrap().push((ds_id, open));
            }
        });
        (send, events)
    }

    #[test]
    fn reports_only_changes_and_lost_wins_over_open() {
        let links = Arc::new(Links::default());
        let (send, events) = collector();
        let meta = links.register("a", "meta", send.clone());
        let console = links.register("a", "1", send.clone());
        meta.set(Some(true));
        console.set(Some(true));
        assert_eq!(*events.lock().unwrap(), [("a".to_owned(), true)]);
        console.set(Some(false));
        meta.set(Some(true));
        assert_eq!(events.lock().unwrap().last(), Some(&("a".to_owned(), false)));
        console.set(Some(true));
        assert_eq!(events.lock().unwrap().len(), 3);
        assert_eq!(events.lock().unwrap().last(), Some(&("a".to_owned(), true)));
    }

    #[test]
    fn sessions_dropped_by_disconnect_stay_silent() {
        let links = Arc::new(Links::default());
        let (send, events) = collector();
        let stale = links.register("a", "1", send.clone());
        links.forget_source("a");
        stale.set(Some(true));
        // Reusing the name after Disconnect does not revive the old session.
        let fresh = links.register("a", "1", send);
        stale.set(Some(false));
        assert!(events.lock().unwrap().is_empty());
        fresh.set(Some(true));
        assert_eq!(*events.lock().unwrap(), [("a".to_owned(), true)]);
    }

    #[test]
    fn closing_the_lost_console_restores_the_source() {
        let links = Arc::new(Links::default());
        let (send, events) = collector();
        links.register("a", "meta", send.clone()).set(Some(true));
        links.register("a", "7", send).set(Some(false));
        assert_eq!(links.forget_session("a", "7"), Some(true));
        assert_eq!(events.lock().unwrap().last(), Some(&("a".to_owned(), false)));
    }
}
