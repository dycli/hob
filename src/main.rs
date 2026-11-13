//! The house's backend: one service that reads the house's sources and
//! controls its devices, with any number of screens over it. Each domain is
//! a module behind one versioned HTTP API (`/api/v1`, api.rs) and the same
//! functions over MCP for agents (mcp.rs). The screens hold no logic of the
//! house's: a wall frame's page (ui/frame.html) and the cards an MCP Apps
//! host draws (ui/cameras.html, ui/overview.html). Any other
//! screen reads the same API.
//!
//! Frigate is the camera engine and Immich the photo library; this service
//! stands where Home Assistant would, and grows by adding modules (Tapo,
//! Z-Wave, automations) beside these.
//!
//! I/O is blocking throughout, a thread per connection: every upstream
//! client here (ureq, tungstenite, image) blocks, a house has a handful of
//! clients, and one model with no async runtime keeps the code and the
//! binary small.
//!
//! Environment:
//! - `HOB_LISTEN`: address to bind, e.g. `0.0.0.0:8970`.
//! - `HOB_TZ`: the house's time zone, e.g. `Europe/London`.
//! - `HOB_PUBLIC` (optional): the address MCP hosts reach this server at,
//!   e.g. `https://home.example.net`, where the cards load pictures.
//! - `HOB_IMMICH`, `HOB_IMMICH_KEY_FILE`, `HOB_ALBUM` (together,
//!   optional): Immich's base URL, an API key with album.read, asset.read
//!   and asset.view, and the album the house shows.
//! - `CACHE_DIRECTORY` (optional, set by systemd): where fitted photos are
//!   kept between requests and restarts.
//! - `HOB_CALDAV_FILE` (optional): CalDAV accounts,
//!   `[{"name","url","username","password"}]`.
//! - `HOB_WEATHER` (optional): `latitude,longitude` for the forecast.
//! - `HOB_AIR` (optional): the AirGradient monitor's address.
//! - `HOB_CAMERAS` (optional): a JSON list of `{"id", "name", "turns"}`,
//!   by their Frigate and go2rtc name, with `HOB_GO2RTC` and
//!   `HOB_FRIGATE`, both on loopback.

mod air;
mod api;
mod calendar;
mod cameras;
mod core;
mod http;
mod mcp;
mod photos;
mod weather;

use std::sync::Arc;

use air::Air;
use calendar::Calendar;
use cameras::Cameras;
use core::Health;
use photos::Photos;
use weather::Weather;

/// The house: each module it has, absent where the host configures none.
pub struct App {
    pub zone: chrono_tz::Tz,
    pub public: Option<String>,
    pub photos: Option<Photos>,
    pub weather: Option<Weather>,
    pub air: Option<Air>,
    pub calendar: Option<Calendar>,
    pub cameras: Option<Cameras>,
    /// When this server started; a screen that sees it change reloads, so
    /// a deploy reaches the wall without anyone touching the frame.
    pub boot: u64,
}

impl App {
    pub fn health(&self) -> Vec<Health> {
        let mut h = Vec::new();
        h.extend(self.photos.as_ref().map(Photos::health));
        h.extend(self.weather.as_ref().map(Weather::health));
        h.extend(self.air.as_ref().map(Air::health));
        h.extend(self.calendar.as_ref().map(Calendar::health));
        h.extend(self.cameras.as_ref().map(Cameras::health));
        h
    }
}

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn file(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Ends hob at once on SIGTERM or SIGINT. As a container's first process it
/// would otherwise get no default handling of either, and every stop would
/// wait out the runtime's grace period. There is nothing to save: the photo
/// store writes each file aside and renames it into place.
fn end_on_signals() {
    extern "C" fn end(_: libc::c_int) {
        // SAFETY: _exit is async-signal-safe.
        unsafe { libc::_exit(0) }
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: `end` is an extern "C" handler that only calls _exit.
        unsafe { libc::signal(signal, end as *const () as libc::sighandler_t) };
    }
}

fn main() {
    end_on_signals();
    let zone_name = env("HOB_TZ");
    let zone: chrono_tz::Tz = zone_name.parse().unwrap_or_else(|e| panic!("HOB_TZ: {e}"));
    let app = Arc::new(App {
        zone,
        public: optional("HOB_PUBLIC").map(|p| p.trim_end_matches('/').to_owned()),
        photos: optional("HOB_IMMICH").map(|base| {
            Photos::new(
                &base,
                file(&env("HOB_IMMICH_KEY_FILE")).trim().to_owned(),
                env("HOB_ALBUM"),
                optional("CACHE_DIRECTORY").as_deref(),
            )
        }),
        weather: optional("HOB_WEATHER")
            .map(|p| Weather::new(&p, &zone_name).unwrap_or_else(|e| panic!("{e}"))),
        air: optional("HOB_AIR").map(|host| Air::new(&host)),
        calendar: optional("HOB_CALDAV_FILE")
            .map(|f| Calendar::new(&file(&f), &zone_name).unwrap_or_else(|e| panic!("{e}"))),
        cameras: optional("HOB_CAMERAS").map(|list| {
            Cameras::new(&list, &env("HOB_GO2RTC"), &env("HOB_FRIGATE"))
                .unwrap_or_else(|e| panic!("{e}"))
        }),
        boot: core::unix(),
    });

    let listen = env("HOB_LISTEN");
    let server = tiny_http::Server::http(&listen).unwrap_or_else(|e| panic!("{listen}: {e}"));
    for mut request in server.incoming_requests() {
        let app = app.clone();
        std::thread::spawn(move || {
            let req = http::Request::read(&mut request);
            api::route(&app, &req).send(request);
        });
    }
}
