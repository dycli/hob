//! The cameras domain: the house's cameras, their live view, pan and tilt,
//! and what Frigate saw on them. Every UI (the frame, the camera card) and
//! every agent reaches the cameras through these functions, by way of the
//! HTTP API (api.rs) or MCP (mcp.rs); neither adds logic of its own.
//!
//! Frigate stays the engine. Its own UI builds the timeline from review
//! items, stretches of activity on one camera graded `alert` or
//! `detection`, not from single tracked objects, and so does this: an
//! `Event` here is one review item. Clips are cut from the recordings by
//! the item's time span, as Frigate's review pane plays them. go2rtc
//! serves the live view; Frigate's websocket turns the cameras that turn.
//! Only the cameras configured here are shown, whatever else Frigate has.

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::core::{Fault, Health, Tracker, now};

/// A camera: its Frigate and go2rtc name, the label UIs show, and whether
/// it pans and tilts.
#[derive(Clone, Deserialize, Serialize)]
pub struct Camera {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub turns: bool,
}

/// One stretch of activity on one camera, as Frigate grades it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    pub id: String,
    pub camera: String,
    /// Unix seconds.
    pub start: f64,
    /// None while the activity goes on.
    pub end: Option<f64>,
    /// `alert` or `detection`.
    pub severity: String,
    /// What Frigate saw: `person`, `dog`, `car`…
    pub objects: Vec<String>,
    pub zones: Vec<String>,
    pub reviewed: bool,
}

/// Which events to list.
#[derive(Default, Deserialize)]
pub struct Query {
    pub camera: Option<String>,
    /// Unix seconds.
    pub after: Option<f64>,
    pub before: Option<f64>,
    pub severity: Option<String>,
    pub limit: Option<u32>,
}

/// The most events one listing returns.
pub const MOST: u32 = 200;
/// How many when the caller does not say.
const SOME: u32 = 50;

/// A pan or tilt.
#[derive(Clone, Copy)]
pub enum Way {
    Left,
    Right,
    Up,
    Down,
}

impl Way {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "left" => Way::Left,
            "right" => Way::Right,
            "up" => Way::Up,
            "down" => Way::Down,
            _ => return None,
        })
    }

    fn command(self) -> &'static str {
        match self {
            Way::Left => "MOVE_LEFT",
            Way::Right => "MOVE_RIGHT",
            Way::Up => "MOVE_UP",
            Way::Down => "MOVE_DOWN",
        }
    }
}

/// A body to relay as it arrives: its type and its length when known.
pub struct Stream {
    pub kind: String,
    pub length: Option<String>,
    pub body: Box<dyn Read + Send>,
}

/// How long a camera turns after one move. A UI repeats the move while an
/// arrow is held, so the server always sends the stop itself and a dropped
/// connection cannot leave a camera turning.
pub const NUDGE: Duration = Duration::from_millis(500);

pub struct Cameras {
    list: Vec<Camera>,
    go2rtc: String,
    frigate: String,
    /// Bumped by each move, so only the latest move's timer stops the camera.
    moves: AtomicU64,
    /// For answers that end: lists, pictures.
    agent: ureq::Agent,
    /// Connects with a timeout but reads forever: a live stream never ends.
    streams: ureq::Agent,
    tracker: Tracker,
}

/// Frigate's ids are `<unix time>-<random>`; anything else never reaches
/// its URL.
pub fn is_event_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && !s.starts_with('.')
}

impl Cameras {
    /// `list` as JSON, go2rtc's API and Frigate's (both on loopback).
    pub fn new(list: &str, go2rtc: &str, frigate: &str) -> Result<Self, String> {
        Ok(Cameras {
            list: serde_json::from_str(list).map_err(|e| format!("cameras: {e}"))?,
            go2rtc: go2rtc.trim_end_matches('/').to_owned(),
            frigate: frigate.trim_end_matches('/').to_owned(),
            moves: AtomicU64::new(0),
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(30)))
                .build()
                .into(),
            streams: ureq::Agent::config_builder()
                .timeout_connect(Some(Duration::from_secs(5)))
                .timeout_recv_response(Some(Duration::from_secs(60)))
                .build()
                .into(),
            tracker: Tracker::default(),
        })
    }

    pub fn list(&self) -> &[Camera] {
        &self.list
    }

    /// Frigate's and go2rtc's last answers to this module.
    pub fn health(&self) -> Health {
        self.tracker.health("cameras")
    }

    /// A call upstream, noted for health.
    fn upstream<T>(&self, r: Result<T, String>) -> Result<T, Fault> {
        self.tracker.record(r).map_err(Fault::Unavailable)
    }

    fn camera(&self, id: &str) -> Result<&Camera, Fault> {
        self.list
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(|| Fault::Unknown(format!("no camera {id:?}")))
    }

    /// The camera's latest frame as JPEG, `height` pixels high if given.
    pub fn snapshot(&self, id: &str, height: Option<u32>) -> Result<Vec<u8>, Fault> {
        self.camera(id)?;
        let mut req = self
            .agent
            .get(format!("{}/api/{id}/latest.jpg", self.frigate));
        if let Some(h) = height {
            req = req.query("h", h.clamp(90, 2160).to_string());
        }
        self.upstream(
            req.call()
                .and_then(|mut r| r.body_mut().with_config().limit(16 << 20).read_to_vec())
                .map_err(|e| format!("snapshot {id}: {e}")),
        )
    }

    /// Events newest first, on the configured cameras only.
    pub fn events(&self, q: &Query) -> Result<Vec<Event>, Fault> {
        let cameras = match &q.camera {
            Some(c) => self.camera(c)?.id.clone(),
            None => self
                .list
                .iter()
                .map(|c| c.id.as_str())
                .collect::<Vec<_>>()
                .join(","),
        };
        let mut req = self
            .agent
            .get(format!("{}/api/review", self.frigate))
            .query("cameras", cameras)
            .query("limit", q.limit.unwrap_or(SOME).clamp(1, MOST).to_string());
        if let Some(s) = &q.severity {
            if s != "alert" && s != "detection" {
                return Err(Fault::Invalid(format!("no severity {s:?}")));
            }
            req = req.query("severity", s);
        }
        if let Some(a) = q.after {
            req = req.query("after", a.to_string());
        }
        if let Some(b) = q.before {
            req = req.query("before", b.to_string());
        }
        let items: Vec<Value> = self.upstream(
            req.call()
                .and_then(|mut r| r.body_mut().read_json())
                .map_err(|e| format!("events: {e}")),
        )?;
        Ok(items
            .iter()
            .filter_map(event)
            .filter(|e| self.list.iter().any(|c| c.id == e.camera))
            .collect())
    }

    /// One event by its id.
    pub fn event(&self, id: &str) -> Result<Event, Fault> {
        if !is_event_id(id) {
            return Err(Fault::Unknown(format!("no event {id:?}")));
        }
        let item: Value = match self
            .agent
            .get(format!("{}/api/review/{id}", self.frigate))
            .call()
        {
            Ok(mut r) => r
                .body_mut()
                .read_json()
                .map_err(|e| Fault::Unavailable(format!("event {id}: {e}")))?,
            Err(ureq::Error::StatusCode(404)) => {
                return Err(Fault::Unknown(format!("no event {id:?}")));
            }
            Err(e) => return Err(Fault::Unavailable(format!("event {id}: {e}"))),
        };
        event(&item)
            .filter(|e| self.list.iter().any(|c| c.id == e.camera))
            .ok_or_else(|| Fault::Unknown(format!("no event {id:?}")))
    }

    /// The event's picture, as Frigate keeps it for its review pane (WebP).
    pub fn thumbnail(&self, id: &str) -> Result<Vec<u8>, Fault> {
        let e = self.event(id)?;
        self.upstream(
            self.agent
                .get(format!(
                    "{}/clips/review/thumb-{}-{}.webp",
                    self.frigate, e.camera, e.id
                ))
                .call()
                .and_then(|mut r| r.body_mut().with_config().limit(8 << 20).read_to_vec())
                .map_err(|e| format!("thumbnail {id}: {e}")),
        )
    }

    /// The event's recording as MP4, cut from the camera's recordings by
    /// the event's span. Frigate answers no Range request for a cut clip,
    /// so neither does this; a player seeks in what it has loaded.
    pub fn clip(&self, id: &str) -> Result<Stream, Fault> {
        let e = self.event(id)?;
        let end = e.end.unwrap_or_else(now);
        let r = self.upstream(
            self.streams
                .get(format!(
                    "{}/api/{}/start/{}/end/{}/clip.mp4",
                    self.frigate,
                    e.camera,
                    e.start.floor() as i64,
                    end.ceil() as i64
                ))
                .call()
                .map_err(|err| format!("clip {id}: {err}")),
        )?;
        Ok(stream(r, "video/mp4"))
    }

    /// The camera's sub-stream live as fragmented MP4, for as long as the
    /// caller reads.
    pub fn live(&self, id: &str) -> Result<Stream, Fault> {
        self.camera(id)?;
        let r = self.upstream(
            self.streams
                .get(format!("{}/api/stream.mp4", self.go2rtc))
                .query("src", format!("{id}_sub"))
                .call()
                .map_err(|e| format!("camera {id}: {e}")),
        )?;
        let mut s = stream(r, "video/mp4");
        // A live stream has no length; go2rtc's header would only mislead.
        s.length = None;
        Ok(s)
    }

    /// Starts a move and returns its number, which `stop` takes after
    /// NUDGE. Frigate has no HTTP route for continuous moves, so this is
    /// its websocket, as its own UI sends them.
    pub fn turn(&self, id: &str, way: Way) -> Result<u64, Fault> {
        let c = self.camera(id)?;
        if !c.turns {
            return Err(Fault::Invalid(format!("camera {id:?} does not turn")));
        }
        let this = self.moves.fetch_add(1, Ordering::SeqCst) + 1;
        self.upstream(ptz(&self.websocket(), id, way.command()))?;
        Ok(this)
    }

    /// Stops the camera, unless a later move has started since `this`.
    pub fn stop(&self, id: &str, this: u64) -> Result<(), String> {
        if self.moves.load(Ordering::SeqCst) != this {
            return Ok(());
        }
        ptz(&self.websocket(), id, "STOP")
    }

    fn websocket(&self) -> String {
        let base = self
            .frigate
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1);
        format!("{base}/ws")
    }
}

/// A Frigate review item as an Event; None if it lacks what one needs.
fn event(item: &Value) -> Option<Event> {
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    Some(Event {
        id: item["id"].as_str().filter(|i| is_event_id(i))?.to_owned(),
        camera: item["camera"].as_str()?.to_owned(),
        start: item["start_time"].as_f64()?,
        end: item["end_time"].as_f64(),
        severity: item["severity"].as_str()?.to_owned(),
        objects: strings(&item["data"]["objects"]),
        zones: strings(&item["data"]["zones"]),
        reviewed: item["has_been_reviewed"].as_bool().unwrap_or(false),
    })
}

fn stream(r: ureq::http::Response<ureq::Body>, kind: &str) -> Stream {
    let header = |name: &str| {
        r.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    Stream {
        kind: header("content-type").unwrap_or_else(|| kind.to_owned()),
        length: header("content-length"),
        body: Box::new(r.into_body().into_reader()),
    }
}

/// Sends Frigate one PTZ command for a camera over its websocket.
fn ptz(url: &str, camera: &str, command: &str) -> Result<(), String> {
    let (mut ws, _) = tungstenite::connect(url).map_err(|e| format!("frigate: {e}"))?;
    let msg = json!({ "topic": format!("{camera}/ptz"), "payload": command, "retain": false });
    ws.send(tungstenite::Message::text(msg.to_string()))
        .and_then(|()| ws.close(None))
        .map_err(|e| format!("frigate: {e}"))?;
    // Read until Frigate answers the close, so the command is not cut off.
    while ws.read().is_ok() {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_frigate_ids_are_event_ids() {
        assert!(is_event_id("1700000000.123456-a1b2c3"));
        assert!(!is_event_id("../api/config"));
        assert!(!is_event_id("a/b"));
        assert!(!is_event_id(""));
        assert!(!is_event_id(".."));
    }

    #[test]
    fn a_review_item_reads_as_an_event() {
        let item = json!({"id":"1700000000.123456-a1b2c3","camera":"front-door",
            "start_time":1700000000.123456,"end_time":null,"severity":"alert",
            "data":{"objects":["person","dog"],"zones":[]},"has_been_reviewed":false});
        let e = event(&item).unwrap();
        assert_eq!(e.camera, "front-door");
        assert_eq!(e.end, None);
        assert_eq!(e.objects, ["person", "dog"]);
        assert!(
            event(&json!({"id":"x/y","camera":"c","start_time":1.0,"severity":"alert"})).is_none()
        );
    }

    #[test]
    fn ways_parse() {
        assert!(Way::parse("left").is_some());
        assert!(Way::parse("spin").is_none());
    }
}
