//! Weather from Open-Meteo, which needs no key: now, the next day by the
//! hour and the next week by the day for one place, in °F and mph.

use serde::{Deserialize, Serialize};
use std::time::Duration;
use ureq::Agent;

use crate::core::{Cached, Fault, Health};

/// Open-Meteo updates its model hourly; a quarter hour keeps the current
/// conditions close.
const FRESH: Duration = Duration::from_secs(900);

const DAYS: u32 = 7;
/// Hours from the current one, for the day's chart.
const HOURS: u32 = 25;

pub struct Weather {
    url: String,
    agent: Agent,
    held: Cached<Report>,
}

/// What the page draws. `code` is the WMO weather code Open-Meteo uses.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Report {
    pub now: Now,
    pub hours: Vec<Hour>,
    pub days: Vec<Day>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Now {
    pub temp: f64,
    pub feels: f64,
    pub code: u8,
    pub day: bool,
    pub humidity: f64,
    pub dew: f64,
    pub wind: f64,
    pub gust: f64,
    /// Degrees the wind blows from.
    pub from: f64,
    pub uv: f64,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Hour {
    pub time: String,
    pub temp: f64,
    pub code: u8,
    pub day: bool,
    pub rain: Option<u8>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Day {
    pub date: String,
    pub code: u8,
    pub high: f64,
    pub low: f64,
    pub rain: Option<u8>,
    /// Inches expected.
    pub fall: f64,
    pub sunrise: String,
    pub sunset: String,
    pub uv: Option<f64>,
}

#[derive(Deserialize)]
struct Answer {
    current: Current,
    hourly: Hourly,
    daily: Daily,
}

#[derive(Deserialize)]
struct Current {
    temperature_2m: f64,
    apparent_temperature: f64,
    weather_code: u8,
    is_day: u8,
    relative_humidity_2m: f64,
    dew_point_2m: f64,
    wind_speed_10m: f64,
    wind_gusts_10m: f64,
    wind_direction_10m: f64,
    uv_index: f64,
}

#[derive(Deserialize)]
struct Hourly {
    time: Vec<String>,
    temperature_2m: Vec<f64>,
    weather_code: Vec<u8>,
    is_day: Vec<u8>,
    precipitation_probability: Vec<Option<u8>>,
}

#[derive(Deserialize)]
struct Daily {
    time: Vec<String>,
    weather_code: Vec<u8>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
    precipitation_probability_max: Vec<Option<u8>>,
    precipitation_sum: Vec<Option<f64>>,
    sunrise: Vec<String>,
    sunset: Vec<String>,
    uv_index_max: Vec<Option<f64>>,
}

impl Weather {
    /// For a place given as `latitude,longitude`, days in zone `zone`.
    pub fn new(place: &str, zone: &str) -> Result<Self, String> {
        let (lat, lon) = place
            .split_once(',')
            .and_then(|(a, b)| Some((a.trim().parse::<f64>().ok()?, b.trim().parse::<f64>().ok()?)))
            .ok_or("HOB_WEATHER is latitude,longitude")?;
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}\
             &current=temperature_2m,apparent_temperature,weather_code,is_day,relative_humidity_2m,\
             dew_point_2m,wind_speed_10m,wind_gusts_10m,wind_direction_10m,uv_index\
             &hourly=temperature_2m,weather_code,is_day,precipitation_probability\
             &daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,\
             precipitation_sum,sunrise,sunset,uv_index_max\
             &temperature_unit=fahrenheit&wind_speed_unit=mph&precipitation_unit=inch\
             &timezone={zone}&forecast_days={DAYS}&forecast_hours={HOURS}"
        );
        let agent = Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        Ok(Weather {
            url,
            agent,
            held: Cached::new(FRESH),
        })
    }

    /// The forecast as last fetched within FRESH.
    pub fn report(&self) -> Result<Report, Fault> {
        self.held.get(|| self.fetch())
    }

    pub fn health(&self) -> Health {
        self.held.tracker.health("weather")
    }

    fn fetch(&self) -> Result<Report, String> {
        let answer: Answer = self
            .agent
            .get(&self.url)
            .call()
            .and_then(|mut r| r.body_mut().read_json())
            .map_err(|e| format!("weather: {e}"))?;
        Ok(read(answer))
    }
}

fn read(a: Answer) -> Report {
    let h = a.hourly;
    let hours = (0..h.time.len())
        .filter_map(|i| {
            Some(Hour {
                time: h.time.get(i)?.clone(),
                temp: *h.temperature_2m.get(i)?,
                code: *h.weather_code.get(i)?,
                day: *h.is_day.get(i)? == 1,
                rain: h.precipitation_probability.get(i).copied().flatten(),
            })
        })
        .collect();
    let d = a.daily;
    let days = (0..d.time.len())
        .filter_map(|i| {
            Some(Day {
                date: d.time.get(i)?.clone(),
                code: *d.weather_code.get(i)?,
                high: *d.temperature_2m_max.get(i)?,
                low: *d.temperature_2m_min.get(i)?,
                rain: d.precipitation_probability_max.get(i).copied().flatten(),
                fall: d.precipitation_sum.get(i).copied().flatten().unwrap_or(0.0),
                sunrise: d.sunrise.get(i)?.clone(),
                sunset: d.sunset.get(i)?.clone(),
                uv: d.uv_index_max.get(i).copied().flatten(),
            })
        })
        .collect();
    let c = a.current;
    Report {
        now: Now {
            temp: c.temperature_2m,
            feels: c.apparent_temperature,
            code: c.weather_code,
            day: c.is_day == 1,
            humidity: c.relative_humidity_2m,
            dew: c.dew_point_2m,
            wind: c.wind_speed_10m,
            gust: c.wind_gusts_10m,
            from: c.wind_direction_10m,
            uv: c.uv_index,
        },
        hours,
        days,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_reads_into_now_and_days() {
        let a: Answer = serde_json::from_str(
            r#"{"current":{"time":"2026-10-16T14:00","temperature_2m":58.3,"apparent_temperature":55.1,"weather_code":2,"is_day":1,
                  "relative_humidity_2m":64,"dew_point_2m":32.8,"wind_speed_10m":1.8,"wind_gusts_10m":2.9,"wind_direction_10m":211,"uv_index":3.4},
                "hourly":{"time":["2026-10-16T14:00","2026-10-16T15:00"],"temperature_2m":[58.3,57.0],"weather_code":[2,3],"is_day":[1,1],"precipitation_probability":[0,null]},
                "daily":{"time":["2026-10-16","2026-10-17"],"weather_code":[2,61],"temperature_2m_max":[61.0,55.2],"temperature_2m_min":[44.1,47.0],"precipitation_probability_max":[5,null],
                  "precipitation_sum":[0.0,0.31],"sunrise":["2026-10-16T07:00","2026-10-17T07:01"],"sunset":["2026-10-16T17:55","2026-10-17T17:53"],"uv_index_max":[4.25,null]}}"#,
        )
        .unwrap();
        let r = read(a);
        assert_eq!(r.now.code, 2);
        assert!(r.now.day);
        assert_eq!(r.days.len(), 2);
        assert_eq!(r.days[1].code, 61);
        assert_eq!(r.days[1].rain, None);
        assert_eq!(r.days[1].fall, 0.31);
        assert_eq!(r.hours.len(), 2);
        assert_eq!(r.hours[1].rain, None);
        assert_eq!(r.now.from, 211.0);
    }
}
