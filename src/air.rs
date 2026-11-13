//! Indoor air from an AirGradient monitor's local API, which needs no key:
//! CO₂, temperature in °F, humidity, dust, and the VOC and NOx indexes,
//! as the monitor shows them (its compensated readings where it has them).

use serde::{Deserialize, Serialize};
use std::time::Duration;
use ureq::Agent;

use crate::core::{Cached, Fault, Health};

/// The monitor itself updates about once a minute.
const FRESH: Duration = Duration::from_secs(60);

pub struct Air {
    url: String,
    agent: Agent,
    held: Cached<Reading>,
}

/// What the page draws.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Reading {
    pub co2: f64,
    pub temp: f64,
    pub humidity: f64,
    pub pm25: f64,
    pub pm1: f64,
    pub pm10: f64,
    /// Sensirion's indexes: VOC 100 is the room's usual, NOx 1 is clean.
    /// The monitor leaves them out while the sensor warms up.
    pub voc: Option<f64>,
    pub nox: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Measures {
    /// A whole number on some firmware, an average with decimals on others.
    rco2: f64,
    atmp_compensated: f64,
    rhum_compensated: f64,
    pm02_compensated: f64,
    pm01: f64,
    pm10: f64,
    tvoc_index: Option<f64>,
    nox_index: Option<f64>,
}

impl Air {
    /// For the monitor at `host`, e.g. `192.0.2.10`.
    pub fn new(host: &str) -> Self {
        let agent = Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        Air {
            url: format!("http://{host}/measures/current"),
            agent,
            held: Cached::new(FRESH),
        }
    }

    /// The room as last read within FRESH.
    pub fn reading(&self) -> Result<Reading, Fault> {
        self.held.get(|| self.fetch())
    }

    pub fn health(&self) -> Health {
        self.held.tracker.health("air")
    }

    fn fetch(&self) -> Result<Reading, String> {
        let m: Measures = self
            .agent
            .get(&self.url)
            .call()
            .and_then(|mut r| r.body_mut().read_json())
            .map_err(|e| format!("air: {e}"))?;
        Ok(read(m))
    }
}

fn read(m: Measures) -> Reading {
    Reading {
        co2: m.rco2,
        temp: m.atmp_compensated * 9.0 / 5.0 + 32.0,
        humidity: m.rhum_compensated,
        pm25: m.pm02_compensated,
        pm1: m.pm01,
        pm10: m.pm10,
        voc: m.tvoc_index,
        nox: m.nox_index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_read_into_a_reading() {
        let m: Measures = serde_json::from_str(
            r#"{"pm01":1.5,"pm02":0,"pm10":4,"pm02Compensated":3.2,"atmp":19.46,"atmpCompensated":20.0,"rhum":66.43,"rhumCompensated":61.5,"rco2":829.33,"tvocIndex":10.67,"model":"I-9PSL"}"#,
        )
        .unwrap();
        let r = read(m);
        assert_eq!(r.co2, 829.33);
        assert!((r.temp - 68.0).abs() < 1e-9);
        assert_eq!(r.humidity, 61.5);
        assert_eq!(r.pm25, 3.2);
        assert_eq!((r.pm1, r.pm10), (1.5, 4.0));
        assert_eq!((r.voc, r.nox), (Some(10.67), None));
    }
}
