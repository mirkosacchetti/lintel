//! The night light: gammastep's user unit, on or off from systemd; while
//! on, the period and the colour temperature computed here the way
//! gammastep does (solar elevation at the location in its config.ini,
//! day above 3°, night below -6°, a linear blend between). Taken on the
//! unit's PropertiesChanged, `lintel refresh` and when the card opens.
//!
//! {on: bool, info: "Daytime, 6500K" while on, "Off" when off}

use super::dbus;
use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};

const UNIT: &str = "gammastep.service";

struct Settings {
    lat: f64,
    lon: f64,
    temp_day: f64,
    temp_night: f64,
    elevation_high: f64,
    elevation_low: f64,
}

fn settings() -> Settings {
    let mut s = Settings {
        lat: 0.0,
        lon: 0.0,
        temp_day: 6500.0,
        temp_night: 4500.0,
        elevation_high: 3.0,
        elevation_low: -6.0,
    };
    let path = crate::config::config_dir().parent().map(|p| p.join("gammastep/config.ini"));
    let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else {
        return s;
    };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v: f64 = match v.trim().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        match k.trim() {
            "lat" => s.lat = v,
            "lon" => s.lon = v,
            "temp-day" => s.temp_day = v,
            "temp-night" => s.temp_night = v,
            "elevation-high" => s.elevation_high = v,
            "elevation-low" => s.elevation_low = v,
            _ => {}
        }
    }
    s
}

/// The sun's elevation in degrees at `unix` seconds (NOAA's algorithm).
fn solar_elevation(unix: i64, lat: f64, lon: f64) -> f64 {
    let jd = unix as f64 / 86400.0 + 2440587.5;
    let t = (jd - 2451545.0) / 36525.0;
    let l0 = (280.46646 + t * (36000.76983 + t * 0.0003032)).rem_euclid(360.0);
    let m = (357.52911 + t * (35999.05029 - 0.0001537 * t)).to_radians();
    let c = (1.914602 - t * (0.004817 + 0.000014 * t)) * m.sin() + (0.019993 - 0.000101 * t) * (2.0 * m).sin() + 0.000289 * (3.0 * m).sin();
    let lambda = (l0 + c).to_radians();
    let eps = (23.0 + (26.0 + (21.448 - t * (46.815 + t * (0.00059 - t * 0.001813))) / 60.0) / 60.0).to_radians();
    let decl = (eps.sin() * lambda.sin()).asin();
    let e = 0.016708634 - t * (0.000042037 + 0.0000001267 * t);
    let y = (eps / 2.0).tan().powi(2);
    let l0r = l0.to_radians();
    let eqtime = 4.0
        * (y * (2.0 * l0r).sin() - 2.0 * e * m.sin() + 4.0 * e * y * m.sin() * (2.0 * l0r).cos()
            - 0.5 * y * y * (4.0 * l0r).sin()
            - 1.25 * e * e * (2.0 * m).sin())
        .to_degrees();
    let minutes = (unix.rem_euclid(86400)) as f64 / 60.0;
    let tst = (minutes + eqtime + 4.0 * lon).rem_euclid(1440.0);
    let ha = (tst / 4.0 - 180.0).to_radians();
    let lat = lat.to_radians();
    (lat.sin() * decl.sin() + lat.cos() * decl.cos() * ha.cos()).asin().to_degrees()
}

/// "Daytime, 6500K", "Night, 4500K" or "Transition (40%), 5300K".
fn period(s: &Settings) -> String {
    let elev = solar_elevation(Utc::now().timestamp(), s.lat, s.lon);
    if elev >= s.elevation_high {
        format!("Daytime, {}K", s.temp_day as i64)
    } else if elev <= s.elevation_low {
        format!("Night, {}K", s.temp_night as i64)
    } else {
        let a = (elev - s.elevation_low) / (s.elevation_high - s.elevation_low);
        let temp = s.temp_night + (s.temp_day - s.temp_night) * a;
        format!("Transition ({}%), {}K", (a * 100.0).round() as i64, temp.round() as i64)
    }
}

async fn active() -> bool {
    let Ok(conn) = dbus::session().await else { return false };
    let Ok(reply) = conn
        .call_method(
            Some("org.freedesktop.systemd1"),
            "/org/freedesktop/systemd1",
            Some("org.freedesktop.systemd1.Manager"),
            "GetUnit",
            &(UNIT,),
        )
        .await
    else {
        return false;
    };
    let Ok(path) = reply.body().deserialize::<zbus::zvariant::OwnedObjectPath>() else {
        return false;
    };
    dbus::string(
        dbus::get(
            &conn,
            "org.freedesktop.systemd1",
            path.as_str(),
            "org.freedesktop.systemd1.Unit",
            "ActiveState",
        )
        .await
        .as_ref(),
    ) == "active"
}

pub async fn take() -> Result<Value> {
    if active().await {
        Ok(json!({"on": true, "info": period(&settings())}))
    } else {
        Ok(json!({"on": false, "info": "Off"}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elevation_matches_gammastep() {
        // gammastep -l 51.5:0.0 -p at 2026-10-09 10:10:47 UTC: 28.565°
        let e = solar_elevation(1791540647, 51.5, 0.0);
        assert!((e - 28.565).abs() < 0.3, "{e}");
    }
}
