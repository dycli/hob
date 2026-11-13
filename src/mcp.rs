//! The house over MCP, for any agent: the modules' functions as
//! tools, and two cards, MCP Apps (`io.modelcontextprotocol/ui`, stable
//! 2026-01-26) that MCP Apps hosts draw beside the call: the cameras with
//! their timeline, and an overview of the house.
//!
//! Streamable HTTP at `/mcp`, stateless: each POST is one JSON-RPC message
//! answered as JSON, which the transport allows in place of an event
//! stream, so there are no sessions to keep and a GET has nothing to
//! offer. The protocol surface used here is small (initialize, tools,
//! resources, ping), so it is written out rather than taken from an SDK
//! and its dependencies.
//!
//! Only loopback reaches it: an MCP client on the same host directly, any
//! other through a proxy on the host. Screens on the LAN get the HTTP API
//! they draw from and never the agents' door. A request carrying an Origin
//! is a browser's, which MCP clients are not, and is refused, so no web page
//! can rebind a name onto it.

use std::sync::Arc;

use base64::Engine;
use chrono::TimeZone;
use serde_json::{Value, json};

use crate::App;
use crate::api::nudge;
use crate::cameras::{self, Event, Way};
use crate::core::{Fault, now};
use crate::http::{Request, Response};

/// The protocol versions this server speaks, newest last.
const VERSIONS: &[&str] = &["2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28"];

/// The MCP Apps extension and the one content type it defines.
const EXTENSION: &str = "io.modelcontextprotocol/ui";
const MIME: &str = "text/html;profile=mcp-app";

/// The cards.
const CAMERAS: &str = "ui://hob/cameras";
const OVERVIEW: &str = "ui://hob/overview";

/// How far back the cards and lists reach unless asked.
const DAY: f64 = 24.0 * 3600.0;
/// The most hours one listing may reach back: Frigate keeps about a week.
const WEEK_HOURS: f64 = 7.0 * 24.0;

pub fn post(app: &Arc<App>, req: &Request) -> Response {
    if !req.peer.is_some_and(|p| p.is_loopback()) {
        return Response::empty(404);
    }
    if req.origin {
        return Response::empty(403);
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&req.body) else {
        return Response::json(&error(Value::Null, -32700, "parse error"));
    };
    // A notification or a response wants no answer.
    let Some(id) = msg.get("id").cloned() else {
        return Response::empty(202);
    };
    let Some(method) = msg["method"].as_str() else {
        return Response::empty(202);
    };
    let params = &msg["params"];
    let answer = match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools(app) })),
        "tools/call" => Ok(call(app, params)),
        "resources/list" => Ok(json!({ "resources": resources(app) })),
        "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
        "resources/read" => read(app, params),
        _ => Err((-32601, format!("no method {method}"))),
    };
    Response::json(&match answer {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err((code, m)) => error(id, code, &m),
    })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or_default();
    let version = VERSIONS
        .iter()
        .find(|v| **v == asked)
        .unwrap_or(VERSIONS.last().expect("versions"));
    json!({
        "protocolVersion": version,
        "capabilities": {
            "tools": {},
            "resources": {},
            "extensions": { EXTENSION: {} },
        },
        "serverInfo": { "name": "hob", "title": "hob", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "The house: its cameras and what they saw (Frigate), the weather, the air indoors, the household calendar, and whether each part of the house's backend is working. Times are Unix seconds in data; text answers give the house's local time.",
    })
}

fn card(uri: &str) -> Value {
    json!({ "ui": { "resourceUri": uri } })
}

/// The card of a tool whose card is the point of calling it, so a host
/// shows it with each call: `is.libri/opens`, which Libri reads (MCP Apps
/// has no field for it). A lookup's card (`card`) waits to be asked for.
fn shown_card(uri: &str) -> Value {
    json!({ "ui": { "resourceUri": uri }, "is.libri/opens": true })
}

/// The tools of the modules this house has.
fn tools(app: &App) -> Vec<Value> {
    let mut t = vec![json!({
        "name": "home",
        "title": "Home",
        "description": "The house at a glance: the weather now and today, the air indoors, the calendar for today and tomorrow, the cameras, and any part of the house's backend that is not answering. Shown as a card.",
        "inputSchema": { "type": "object", "properties": {} },
        "annotations": { "readOnlyHint": true },
        "_meta": shown_card(OVERVIEW),
    })];
    if app.weather.is_some() {
        t.push(json!({
            "name": "weather",
            "title": "Weather",
            "description": "The forecast for the house: now, the next day by the hour, the next week by the day, in °F and mph.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        }));
    }
    if app.air.is_some() {
        t.push(json!({
            "name": "air",
            "title": "Air indoors",
            "description": "The air in the house now: CO₂ (ppm), temperature (°F), humidity (%), PM2.5/PM1/PM10 (µg/m³), and the VOC and NOx indexes (VOC 100 is the room's usual; NOx 1 is clean).",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        }));
    }
    if app.calendar.is_some() {
        t.push(json!({
            "name": "calendar",
            "title": "Household calendar",
            "description": "The household calendar from today for the next week, read only.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": { "readOnlyHint": true },
        }));
    }
    if app.cameras.is_some() {
        let camera = json!({ "type": "string", "description": "A camera's id, from `cameras`." });
        t.extend([
            json!({
                "name": "cameras",
                "title": "Cameras",
                "description": "The house's cameras and what they saw in the last day, shown as a card with live views and the timeline.",
                "inputSchema": { "type": "object", "properties": {} },
                "annotations": { "readOnlyHint": true },
                "_meta": shown_card(CAMERAS),
            }),
            json!({
                "name": "camera_events",
                "title": "Camera events",
                "description": "What the cameras saw (people, animals, cars), newest first, from Frigate's review items. An `alert` is something Frigate grades important; a `detection` is lesser.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "camera": camera,
                        "hours": { "type": "number", "description": "How many hours back, at most 168. Default 24." },
                        "severity": { "type": "string", "enum": ["alert", "detection"] },
                        "limit": { "type": "integer", "minimum": 1, "maximum": cameras::MOST },
                    },
                },
                "annotations": { "readOnlyHint": true },
                "_meta": card(CAMERAS),
            }),
            json!({
                "name": "camera_snapshot",
                "title": "Camera snapshot",
                "description": "One camera's latest frame, as an image to look at.",
                "inputSchema": { "type": "object", "properties": { "camera": camera }, "required": ["camera"] },
                "annotations": { "readOnlyHint": true },
            }),
            json!({
                "name": "camera_turn",
                "title": "Turn a camera",
                "description": "Pans or tilts a camera that turns, by a small step.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "camera": camera,
                        "direction": { "type": "string", "enum": ["left", "right", "up", "down"] },
                    },
                    "required": ["camera", "direction"],
                },
                "annotations": { "readOnlyHint": false, "destructiveHint": false },
            }),
        ]);
    }
    t
}

fn resources(app: &App) -> Vec<Value> {
    let mut r = vec![json!({ "uri": OVERVIEW, "name": "home", "title": "Home", "mimeType": MIME })];
    if app.cameras.is_some() {
        r.push(json!({ "uri": CAMERAS, "name": "cameras", "title": "Cameras", "mimeType": MIME }));
    }
    r
}

fn read(app: &App, params: &Value) -> Result<Value, (i64, String)> {
    let uri = params["uri"].as_str().unwrap_or_default();
    let html = match uri {
        OVERVIEW => include_str!("ui/overview.html"),
        CAMERAS if app.cameras.is_some() => include_str!("ui/cameras.html"),
        _ => return Err((-32002, format!("no resource {uri}"))),
    };
    // The cards load pictures and video from hob's public address (HOB_PUBLIC).
    let domains: Vec<&str> = app.public.as_deref().into_iter().collect();
    Ok(json!({ "contents": [{
        "uri": uri,
        "mimeType": MIME,
        "text": html,
        "_meta": { "ui": { "csp": { "resourceDomains": domains }, "prefersBorder": true } },
    }]}))
}

fn failed(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

/// A module's answer as text for a model and data for a card.
fn data(text: String, value: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "structuredContent": value })
}

fn call(app: &Arc<App>, params: &Value) -> Value {
    let args = &params["arguments"];
    let arg = |k: &str| args[k].as_str().map(str::to_owned);
    let absent = |m: &str| Fault::Unknown(format!("This house has no {m} configured."));
    let outcome = match params["name"].as_str().unwrap_or_default() {
        "home" => Ok(overview(app)),
        "weather" => app
            .weather
            .as_ref()
            .ok_or_else(|| absent("weather"))
            .and_then(|w| w.report())
            .map(|r| {
                let v = serde_json::to_value(&r).unwrap_or_default();
                data(format!("Weather (°F, mph): {v}"), v)
            }),
        "air" => app
            .air
            .as_ref()
            .ok_or_else(|| absent("air monitor"))
            .and_then(|a| a.reading())
            .map(|r| {
                let v = serde_json::to_value(&r).unwrap_or_default();
                data(format!("Air indoors: {v}"), v)
            }),
        "calendar" => app
            .calendar
            .as_ref()
            .ok_or_else(|| absent("calendar"))
            .and_then(|c| c.upcoming())
            .map(|events| {
                let mut text = format!("{} events from today:\n", events.len());
                for e in &events {
                    let end = e
                        .end
                        .as_deref()
                        .map(|x| format!(" to {x}"))
                        .unwrap_or_default();
                    let at = e
                        .location
                        .as_deref()
                        .map(|l| format!(" at {l}"))
                        .unwrap_or_default();
                    text += &format!("{}{end}: {}{at} ({})\n", e.start, e.summary, e.calendar);
                }
                data(text, json!({ "events": events }))
            }),
        "cameras" => listing(app, cameras::Query::default(), DAY),
        "camera_events" => {
            let hours = args["hours"]
                .as_f64()
                .unwrap_or(24.0)
                .clamp(0.1, WEEK_HOURS);
            let q = cameras::Query {
                camera: arg("camera"),
                severity: arg("severity"),
                limit: args["limit"]
                    .as_u64()
                    .map(|l| l.min(u64::from(cameras::MOST)) as u32),
                ..Default::default()
            };
            listing(app, q, hours * 3600.0)
        }
        "camera_snapshot" => match arg("camera") {
            None => return failed("Say which camera.".into()),
            Some(id) => app
                .cameras
                .as_ref()
                .ok_or_else(|| absent("cameras"))
                .and_then(|c| c.snapshot(&id, Some(720)))
                .map(|jpeg| {
                    json!({ "content": [{
                        "type": "image",
                        "mimeType": "image/jpeg",
                        "data": base64::engine::general_purpose::STANDARD.encode(jpeg),
                    }]})
                }),
        },
        "camera_turn" => {
            let (Some(id), Some(way)) = (
                arg("camera"),
                arg("direction").as_deref().and_then(Way::parse),
            ) else {
                return failed("Say which camera and which way: left, right, up or down.".into());
            };
            nudge(app, &id, way).map(
                |()| json!({ "content": [{ "type": "text", "text": format!("Turned {id}.") }] }),
            )
        }
        other => return failed(format!("There is no tool {other}.")),
    };
    outcome.unwrap_or_else(|f| match f {
        Fault::Unknown(s) | Fault::Invalid(s) => failed(s),
        Fault::Unavailable(s) => {
            eprintln!("{s}");
            failed("That part of the house did not answer; try again shortly.".into())
        }
    })
}

/// The house at a glance. Each part is there if it answered; what did not
/// is named, and the rest still shows.
fn overview(app: &Arc<App>) -> Value {
    let part = |r: Result<Value, Fault>| r.unwrap_or(Value::Null);
    let weather = app.weather.as_ref().map(|w| {
        part(w.report().map(|r| {
            json!({ "now": r.now, "today": r.days.first(), "hours": r.hours.iter().take(12).collect::<Vec<_>>() })
        }))
    });
    let air = app.air.as_ref().map(|a| {
        part(
            a.reading()
                .map(|r| serde_json::to_value(r).unwrap_or_default()),
        )
    });
    let events = app.calendar.as_ref().map(|c| {
        let tomorrow = chrono::Utc::now()
            .with_timezone(&app.zone)
            .date_naive()
            .succ_opt()
            .map(|d| d.to_string())
            .unwrap_or_default();
        part(c.upcoming().map(|events| {
            json!(
                events
                    .into_iter()
                    .filter(|e| e.start[..10] <= *tomorrow)
                    .collect::<Vec<_>>()
            )
        }))
    });
    let cameras = app.cameras.as_ref().map(|c| json!(c.list()));
    let down: Vec<_> = app
        .health()
        .into_iter()
        .filter(|h| !h.ok && h.error.is_some())
        .collect();

    let mut text = String::new();
    if let Some(w) = weather.as_ref().filter(|w| !w.is_null()) {
        text += &format!(
            "Weather now: {}°F (feels {}°F), WMO code {}; today {}–{}°F.\n",
            w["now"]["temp"],
            w["now"]["feels"],
            w["now"]["code"],
            w["today"]["low"],
            w["today"]["high"]
        );
    }
    if let Some(a) = air.as_ref().filter(|a| !a.is_null()) {
        text += &format!(
            "Air indoors: CO₂ {} ppm, {:.0}°F, {}% humidity, PM2.5 {}.\n",
            a["co2"],
            a["temp"].as_f64().unwrap_or_default(),
            a["humidity"],
            a["pm25"]
        );
    }
    if let Some(ev) = events.as_ref().and_then(Value::as_array) {
        text += &format!("Today and tomorrow: {} events.\n", ev.len());
        for e in ev {
            text += &format!(
                "- {} {}\n",
                e["start"].as_str().unwrap_or_default(),
                e["summary"].as_str().unwrap_or_default()
            );
        }
    }
    if let Some(c) = cameras.as_ref().and_then(Value::as_array) {
        let names: Vec<_> = c.iter().filter_map(|c| c["name"].as_str()).collect();
        text += &format!("Cameras: {}.\n", names.join(", "));
    }
    for h in &down {
        text += &format!(
            "Not answering: {} ({}).\n",
            h.module,
            h.error.as_deref().unwrap_or_default()
        );
    }
    data(
        text,
        json!({
            "base": app.public,
            "weather": weather,
            "air": air,
            "events": events,
            "cameras": cameras,
            "down": down,
        }),
    )
}

/// The cameras and their events over the last `span` seconds: as text for
/// a model, and as data for the card.
fn listing(app: &App, mut q: cameras::Query, span: f64) -> Result<Value, Fault> {
    let cams = app
        .cameras
        .as_ref()
        .ok_or_else(|| Fault::Unknown("This house has no cameras configured.".into()))?;
    q.after = Some(now() - span);
    let events = cams.events(&q)?;
    let list = cams.list();
    let mut text = String::from("Cameras: ");
    text += &list
        .iter()
        .map(|c| {
            format!(
                "{} ({}{})",
                c.name,
                c.id,
                if c.turns { ", turns" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    text += &format!("\n{} events, newest first:\n", events.len());
    for e in &events {
        let name = list
            .iter()
            .find(|c| c.id == e.camera)
            .map_or(e.camera.as_str(), |c| c.name.as_str());
        text += &format!("{}\n", line(e, name, app.zone));
    }
    Ok(data(
        text,
        json!({
            "base": app.public,
            "cameras": list,
            "events": events,
        }),
    ))
}

/// One event as a line: `Tue 14:03 Front door alert: person, dog (12 s) [id]`.
fn line(e: &Event, camera: &str, zone: chrono_tz::Tz) -> String {
    let at = zone
        .timestamp_opt(e.start as i64, 0)
        .single()
        .map_or_else(String::new, |t| t.format("%a %H:%M").to_string());
    let lasted = match e.end {
        Some(end) => format!("{} s", (end - e.start).round() as i64),
        None => "ongoing".into(),
    };
    format!(
        "{at} {camera} {}: {} ({lasted}) [{}]",
        e.severity,
        e.objects.join(", "),
        e.id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_answers_the_asked_version_or_the_newest() {
        let v = initialize(&json!({ "protocolVersion": "2025-06-18" }));
        assert_eq!(v["protocolVersion"], "2025-06-18");
        let v = initialize(&json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(v["protocolVersion"], *VERSIONS.last().unwrap());
        assert!(v["capabilities"]["extensions"][EXTENSION].is_object());
    }

    #[test]
    fn an_event_reads_as_one_line() {
        let e = Event {
            id: "1700000000.123456-a1b2c3".into(),
            camera: "front-door".into(),
            start: 1700000000.6,
            end: Some(1700000012.6),
            severity: "alert".into(),
            objects: vec!["person".into(), "dog".into()],
            zones: vec![],
            reviewed: false,
        };
        let l = line(&e, "Front door", chrono_tz::Europe::London);
        assert!(
            l.ends_with("Front door alert: person, dog (12 s) [1700000000.123456-a1b2c3]"),
            "{l}"
        );
    }
}
