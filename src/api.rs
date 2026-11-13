//! The house's HTTP API, version 1, and the pages served beside it. Every
//! screen reads the house only through `/api/v1`; each route is a thin
//! call into one module.
//!
//! - `GET /api/v1/health`: `{boot, modules: [Health]}`.
//! - `GET /api/v1/photos`: `{photos: [id]}`; `GET /api/v1/photos/{id}`: JPEG.
//! - `GET /api/v1/weather`, `/air`, `/calendar`: each module's reading.
//! - `GET /api/v1/cameras`; `/cameras/{id}/snapshot.jpg?h=`;
//!   `/cameras/{id}/live.mp4`; `POST /cameras/{id}/turn/{left|right|up|down}`.
//! - `GET /api/v1/events?camera=&after=&before=&severity=&limit=`;
//!   `/events/{id}`; `/events/{id}/thumbnail.webp`; `/events/{id}/clip.mp4`.
//!
//! A module the host has not configured answers 404.

use std::sync::Arc;

use serde_json::json;

use crate::App;
use crate::cameras::{self, Way};
use crate::core::Fault;
use crate::http::{Request, Response};

/// A wall frame's page: the photos, the time, the week's calendar, the
/// forecast, the air indoors and the cameras.
fn frame() -> Response {
    Response::bytes(
        200,
        "text/html; charset=utf-8",
        include_str!("ui/frame.html"),
    )
    .cache("no-cache")
}

pub fn failed(f: Fault) -> Response {
    match f {
        Fault::Unknown(s) => Response::text(404, &s),
        Fault::Invalid(s) => Response::text(400, &s),
        Fault::Unavailable(s) => {
            eprintln!("{s}");
            Response::text(502, "unavailable")
        }
    }
}

fn absent(module: &str) -> Fault {
    Fault::Unknown(format!("this house has no {module}"))
}

/// A module's answer as JSON, or its failure.
fn answer<T: serde::Serialize>(r: Result<T, Fault>) -> Response {
    r.map_or_else(failed, |v| Response::json(&v))
}

pub fn route(app: &Arc<App>, req: &Request) -> Response {
    let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    let get = req.method == "GET" || req.method == "HEAD";
    match (req.method.as_str(), &parts[..]) {
        (_, [""]) if get => frame(),
        ("POST", ["mcp"]) => crate::mcp::post(app, req),
        (_, ["mcp"]) => Response::empty(405),
        // The frame's page before this service polled here for its server's
        // start time and reloads when it changes; answering it lets the
        // frame on the wall load the new page by itself. Remove once the
        // frame shows the page that reads /api/v1/health.
        (_, ["api", "board"]) if get => Response::json(&json!({ "boot": app.boot })),
        (_, ["api", "v1", rest @ ..]) => v1(app, req, rest),
        _ => Response::empty(404),
    }
}

fn v1(app: &Arc<App>, req: &Request, parts: &[&str]) -> Response {
    let get = req.method == "GET" || req.method == "HEAD";
    let cams = || app.cameras.as_ref().ok_or_else(|| absent("cameras"));
    match (req.method.as_str(), parts) {
        (_, ["health"]) if get => Response::json(&json!({
            "boot": app.boot,
            "modules": app.health(),
        })),
        (_, ["photos"]) if get => answer(
            app.photos
                .as_ref()
                .ok_or_else(|| absent("photos"))
                .and_then(|p| p.ids())
                .map(|ids| json!({ "photos": ids })),
        ),
        (_, ["photos", id]) if get => match app
            .photos
            .as_ref()
            .ok_or_else(|| absent("photos"))
            .and_then(|p| p.picture(id))
        {
            Ok(jpeg) => Response::bytes(200, "image/jpeg", jpeg).cache("max-age=86400"),
            Err(f) => failed(f),
        },
        (_, ["weather"]) if get => answer(
            app.weather
                .as_ref()
                .ok_or_else(|| absent("weather"))
                .and_then(|w| w.report()),
        ),
        (_, ["air"]) if get => answer(
            app.air
                .as_ref()
                .ok_or_else(|| absent("air"))
                .and_then(|a| a.reading()),
        ),
        (_, ["calendar"]) if get => answer(
            app.calendar
                .as_ref()
                .ok_or_else(|| absent("calendar"))
                .and_then(|c| c.upcoming())
                .map(|events| json!({ "days": crate::calendar::AHEAD, "events": events })),
        ),
        (_, ["cameras"]) if get => answer(cams().map(|c| c.list().to_vec())),
        (_, ["cameras", id, "snapshot.jpg"]) if get => {
            let h = req.number("h").map(|h| h as u32);
            match cams().and_then(|c| c.snapshot(id, h)) {
                Ok(jpeg) => Response::bytes(200, "image/jpeg", jpeg).cache("no-store"),
                Err(f) => failed(f),
            }
        }
        (_, ["cameras", id, "live.mp4"]) if get => {
            stream(cams().and_then(|c| c.live(id)), "no-store")
        }
        ("POST", ["cameras", id, "turn", way]) => {
            let Some(way) = Way::parse(way) else {
                return Response::empty(404);
            };
            match nudge(app, id, way) {
                Ok(()) => Response::empty(204),
                Err(f) => failed(f),
            }
        }
        (_, ["events"]) if get => {
            let q = cameras::Query {
                camera: req.query("camera").map(str::to_owned),
                after: req.number("after"),
                before: req.number("before"),
                severity: req.query("severity").map(str::to_owned),
                limit: req.number("limit").map(|l| l as u32),
            };
            answer(cams().and_then(|c| c.events(&q)))
        }
        (_, ["events", id]) if get => answer(cams().and_then(|c| c.event(id))),
        (_, ["events", id, "thumbnail.webp"]) if get => {
            match cams().and_then(|c| c.thumbnail(id)) {
                // A review item's picture never changes.
                Ok(webp) => Response::bytes(200, "image/webp", webp).cache("max-age=604800"),
                Err(f) => failed(f),
            }
        }
        (_, ["events", id, "clip.mp4"]) if get => {
            stream(cams().and_then(|c| c.clip(id)), "max-age=3600")
        }
        _ => Response::empty(404),
    }
}

fn stream(s: Result<cameras::Stream, Fault>, cache: &str) -> Response {
    match s {
        Ok(s) => {
            let length = s.length.and_then(|l| l.parse().ok());
            Response::stream(&s.kind, s.body, length).cache(cache)
        }
        Err(f) => failed(f),
    }
}

/// Turns a camera for NUDGE, then stops it, unless a later move came
/// first. A screen repeats the move while an arrow is held, so the stop
/// always comes from here and a dropped connection cannot leave a camera
/// turning.
pub fn nudge(app: &Arc<App>, id: &str, way: Way) -> Result<(), Fault> {
    let cams = app.cameras.as_ref().ok_or_else(|| absent("cameras"))?;
    let this = cams.turn(id, way)?;
    let (a, id) = (app.clone(), id.to_owned());
    std::thread::spawn(move || {
        std::thread::sleep(cameras::NUDGE);
        let cams = a.cameras.as_ref().expect("checked above");
        if let Err(e) = cams.stop(&id, this) {
            eprintln!("{e}");
        }
    });
    Ok(())
}
