//! What every domain module shares: how a read is cached and kept through
//! an outage, how a failure is told apart, and how a module reports its
//! health.
//!
//! The contract follows Home Assistant's integrations: each domain owns
//! one coordinator per upstream that fetches on demand at most once per
//! freshness window, keeps the last good answer through an outage, and
//! reports whether it is current (HA's DataUpdateCoordinator and its
//! entities' "unavailable"). Two departures: a domain returns typed data
//! rather than entities in a shared state machine, and a fetch happens when
//! a client asks rather than on a timer, so a house with no screen open
//! costs its upstreams nothing. The entity model arrives with the first
//! domain whose devices change on their own (Z-Wave); it slots in beside
//! these without changing them.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Why a request failed: something that is not here, a request that makes
/// no sense, or an upstream that did not answer.
#[derive(Debug)]
pub enum Fault {
    Unknown(String),
    Invalid(String),
    Unavailable(String),
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fault::Unknown(s) | Fault::Invalid(s) | Fault::Unavailable(s) => f.write_str(s),
        }
    }
}

/// One module's state, as `/api/v1/health` and MCP report it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Health {
    pub module: &'static str,
    /// The last call upstream answered.
    pub ok: bool,
    /// Unix seconds of the last answer upstream gave, if any.
    pub last_good: Option<u64>,
    /// The last failure, while it is the latest word.
    pub error: Option<String>,
}

/// The record of one module's calls upstream, for its health.
#[derive(Default)]
pub struct Tracker {
    state: Mutex<(Option<u64>, Option<String>)>,
}

impl Tracker {
    /// Notes how a call upstream went and passes its result on.
    pub fn record<T>(&self, r: Result<T, String>) -> Result<T, String> {
        let mut s = self.state.lock().expect("tracker");
        match &r {
            Ok(_) => *s = (Some(unix()), None),
            Err(e) => s.1 = Some(e.clone()),
        }
        r
    }

    pub fn health(&self, module: &'static str) -> Health {
        let s = self.state.lock().expect("tracker");
        Health {
            module,
            ok: s.0.is_some() && s.1.is_none(),
            last_good: s.0,
            error: s.1.clone(),
        }
    }
}

/// One upstream read, fetched at most once per `fresh` and kept through
/// failures.
pub struct Cached<T> {
    fresh: Duration,
    held: Mutex<Option<(Instant, T)>>,
    pub tracker: Tracker,
}

impl<T: Clone> Cached<T> {
    pub fn new(fresh: Duration) -> Self {
        Self {
            fresh,
            held: Mutex::new(None),
            tracker: Tracker::default(),
        }
    }

    /// The held answer while fresh, else a new one; when the fetch fails,
    /// the last good answer however old, or the failure if there is none.
    /// Callers wait on one fetch rather than each starting their own.
    pub fn get(&self, fetch: impl FnOnce() -> Result<T, String>) -> Result<T, Fault> {
        let mut held = self.held.lock().expect("cache");
        if let Some((at, v)) = held.as_ref()
            && at.elapsed() < self.fresh
        {
            return Ok(v.clone());
        }
        match self.tracker.record(fetch()) {
            Ok(v) => {
                *held = Some((Instant::now(), v.clone()));
                Ok(v)
            }
            Err(e) => {
                eprintln!("{e}");
                held.as_ref()
                    .map(|(_, v)| v.clone())
                    .ok_or(Fault::Unavailable(e))
            }
        }
    }
}

pub fn unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_answer_is_not_fetched_again() {
        let c = Cached::new(Duration::from_secs(60));
        assert_eq!(c.get(|| Ok(1)).unwrap(), 1);
        assert_eq!(c.get(|| Ok(2)).unwrap(), 1);
    }

    #[test]
    fn a_failure_keeps_the_last_good_answer_and_shows_in_health() {
        let c = Cached::new(Duration::ZERO);
        assert!(matches!(
            c.get(|| Err::<u8, _>("down".into())),
            Err(Fault::Unavailable(_))
        ));
        assert_eq!(c.get(|| Ok(7)).unwrap(), 7);
        assert!(c.tracker.health("t").ok);
        assert_eq!(c.get(|| Err("down".into())).unwrap(), 7);
        let h = c.tracker.health("t");
        assert!(!h.ok);
        assert_eq!(h.error.as_deref(), Some("down"));
        assert!(h.last_good.is_some());
    }
}
