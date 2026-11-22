//! Windows geolocation plus parsing of pasted coordinates / map links.
//!
//! Used by the "Send location" dialog and the location card of the chat view.

use std::fmt;
use std::time::Duration;

/// A position fix from the Windows Location API.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeoFix {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeoError {
    Denied,
    Unavailable,
    Timeout,
    Failed(String),
}

impl fmt::Display for GeoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GeoError::Denied => {
                write!(f, "Location access is off \u{2014} enable it in Windows Settings \u{203A} Privacy \u{203A} Location")
            }
            GeoError::Unavailable => write!(f, "Location is not available on this device"),
            GeoError::Timeout => write!(f, "Timed out while getting the current location"),
            GeoError::Failed(msg) => write!(f, "Could not get the current location: {msg}"),
        }
    }
}

impl std::error::Error for GeoError {}

/// Blocking: asks Windows for permission (showing the OS prompt if needed)
/// and then for a position. Call off the UI thread.
pub fn current_location(timeout: Duration) -> Result<GeoFix, GeoError> {
    use windows::Devices::Geolocation::{GeolocationAccessStatus, Geolocator};
    use windows::Foundation::TimeSpan;

    // HRESULT_FROM_WIN32(ERROR_TIMEOUT)
    const HR_TIMEOUT: i32 = 0x800705B4_u32 as i32;

    let fail = |e: windows::core::Error| {
        if e.code().0 == HR_TIMEOUT {
            GeoError::Timeout
        } else {
            GeoError::Failed(e.message())
        }
    };

    match Geolocator::RequestAccessAsync().and_then(|op| op.join()).map_err(fail)? {
        GeolocationAccessStatus::Allowed => {}
        GeolocationAccessStatus::Denied => return Err(GeoError::Denied),
        _ => return Err(GeoError::Unavailable),
    }

    let locator = Geolocator::new().map_err(fail)?;
    let max_age = TimeSpan { Duration: 0 };
    let wait = TimeSpan { Duration: (timeout.as_nanos() / 100).min(i64::MAX as u128) as i64 };
    let pos = locator
        .GetGeopositionAsyncWithAgeAndTimeout(max_age, wait)
        .and_then(|op| op.join())
        .map_err(fail)?;
    let coord = pos.Coordinate().map_err(fail)?;
    let p = coord.Point().map_err(fail)?.Position().map_err(fail)?;
    let accuracy_m = coord.Accuracy().ok().filter(|a| a.is_finite() && *a >= 0.0);
    log::debug!("geolocation fix near {:.2},{:.2}", p.Latitude, p.Longitude);
    Ok(GeoFix { lat: p.Latitude, lon: p.Longitude, accuracy_m })
}

fn valid(lat: f64, lon: f64) -> Option<(f64, f64)> {
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then_some((lat, lon))
}

/// Exactly two numbers separated by a comma and/or whitespace.
fn parse_pair(s: &str) -> Option<(f64, f64)> {
    let mut it = s.split(|c: char| c == ',' || c.is_whitespace()).filter(|p| !p.is_empty());
    let (a, b) = (it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    valid(a.parse().ok()?, b.parse().ok()?)
}

fn parse_two(a: &str, b: &str) -> Option<(f64, f64)> {
    valid(a.parse().ok()?, b.parse().ok()?)
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        k.eq_ignore_ascii_case(key).then_some(v)
    })
}

/// Parses plain coordinates, `geo:` URIs and Google / OpenStreetMap / Apple
/// map links. Short links are rejected: resolving them would need the network.
pub fn parse_location(input: &str) -> Option<(f64, f64)> {
    let s = input.trim().replace("%2C", ",").replace("%2c", ",").replace("%20", " ").replace('+', " ");
    if s.is_empty() {
        return None;
    }
    if let Some(pair) = parse_pair(&s) {
        return Some(pair);
    }
    if s.len() >= 4 && s.is_char_boundary(4) && s[..4].eq_ignore_ascii_case("geo:") {
        let body = &s[4..];
        let end = body.find([';', '?']).unwrap_or(body.len());
        return parse_pair(&body[..end]);
    }
    if s.to_ascii_lowercase().contains("goo.gl") {
        return None;
    }

    let (before_frag, fragment) = match s.split_once('#') {
        Some((a, b)) => (a, Some(b)),
        None => (s.as_str(), None),
    };
    let (path, query) = match before_frag.split_once('?') {
        Some((a, b)) => (a, Some(b)),
        None => (before_frag, None),
    };

    if let Some(q) = query {
        for key in ["ll", "q", "query", "center", "sll"] {
            if let Some(v) = query_param(q, key) {
                let v = v.strip_prefix("loc:").unwrap_or(v);
                if let Some(pair) = parse_pair(v) {
                    return Some(pair);
                }
            }
        }
        if let (Some(la), Some(lo)) = (query_param(q, "mlat"), query_param(q, "mlon")) {
            if let Some(pair) = parse_two(la, lo) {
                return Some(pair);
            }
        }
    }

    if let Some(idx) = path.find("/@") {
        let seg = path[idx + 2..].split('/').next().unwrap_or("");
        let mut parts = seg.split(',');
        if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
            if let Some(pair) = parse_two(a, b) {
                return Some(pair);
            }
        }
    }

    if let Some(rest) = fragment.and_then(|f| f.strip_prefix("map=")) {
        let mut parts = rest.split('/');
        let _zoom = parts.next()?;
        return parse_two(parts.next()?, parts.next()?);
    }
    None
}

/// Google Maps link for a coordinate (always `.` decimals, 6 places).
pub fn maps_url(lat: f64, lon: f64) -> String {
    format!("https://www.google.com/maps/search/?api=1&query={lat:.6},{lon:.6}")
}

/// "lat, lon" with five decimals (about a metre): what a location card shows
/// and what Copy puts on the clipboard. `parse_location` reads it back.
pub fn coords_text(lat: f64, lon: f64) -> String {
    format!("{lat:.5}, {lon:.5}")
}

/// "Accurate to about 25 m" / "... 1.2 km"; empty when the fix has no accuracy.
pub fn accuracy_text(accuracy_m: Option<f64>) -> String {
    match accuracy_m.filter(|a| a.is_finite() && *a >= 0.0) {
        None => String::new(),
        Some(a) if a < 1000.0 => format!("Accurate to about {} m", a.round().max(1.0)),
        Some(a) => format!("Accurate to about {:.1} km", a / 1000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_display_keeps_five_decimals_and_reads_back() {
        assert_eq!(coords_text(35.6892, 51.389), "35.68920, 51.38900");
        assert_eq!(coords_text(-33.8567844, -151.2152967), "-33.85678, -151.21530");
        assert_eq!(parse_location(&coords_text(48.8584, 2.2945)), Some((48.8584, 2.2945)));
    }

    #[test]
    fn accuracy_wording() {
        assert_eq!(accuracy_text(None), "");
        assert_eq!(accuracy_text(Some(f64::NAN)), "");
        assert_eq!(accuracy_text(Some(0.2)), "Accurate to about 1 m");
        assert_eq!(accuracy_text(Some(24.6)), "Accurate to about 25 m");
        assert_eq!(accuracy_text(Some(1240.0)), "Accurate to about 1.2 km");
    }

    const TEHRAN: (f64, f64) = (35.6892, 51.3890);

    #[test]
    fn plain_pairs() {
        assert_eq!(parse_location("35.6892, 51.3890"), Some(TEHRAN));
        assert_eq!(parse_location("35.6892 51.3890"), Some(TEHRAN));
        assert_eq!(parse_location("  35.6892,51.3890 "), Some(TEHRAN));
        assert_eq!(parse_location("-33.86, -151.2"), Some((-33.86, -151.2)));
    }

    #[test]
    fn geo_uri() {
        assert_eq!(parse_location("geo:35.6,51.3"), Some((35.6, 51.3)));
        assert_eq!(parse_location("geo:35.6,51.3?z=12"), Some((35.6, 51.3)));
        assert_eq!(parse_location("geo:35.6,51.3;u=20"), Some((35.6, 51.3)));
    }

    #[test]
    fn google_links() {
        assert_eq!(parse_location("https://www.google.com/maps/@35.6892,51.3890,15z"), Some(TEHRAN));
        assert_eq!(
            parse_location("https://www.google.com/maps/place/Tehran/@35.6892,51.3890,12z/data=!3m1"),
            Some(TEHRAN)
        );
        assert_eq!(parse_location("https://maps.google.com/?q=35.6,51.3"), Some((35.6, 51.3)));
        assert_eq!(parse_location("https://maps.google.com/?q=35.6%2C51.3"), Some((35.6, 51.3)));
        assert_eq!(parse_location("https://maps.google.com/?ll=35.6,51.3&z=10"), Some((35.6, 51.3)));
        assert_eq!(
            parse_location("https://www.google.com/maps/search/?api=1&query=35.6,51.3"),
            Some((35.6, 51.3))
        );
    }

    #[test]
    fn osm_links() {
        assert_eq!(parse_location("https://www.openstreetmap.org/#map=15/35.6892/51.3890"), Some(TEHRAN));
        assert_eq!(
            parse_location("https://www.openstreetmap.org/?mlat=35.6&mlon=51.3#map=15/1/2"),
            Some((35.6, 51.3))
        );
    }

    #[test]
    fn apple_links() {
        assert_eq!(parse_location("https://maps.apple.com/?ll=35.6,51.3"), Some((35.6, 51.3)));
        assert_eq!(parse_location("https://maps.apple.com/?q=35.6,51.3"), Some((35.6, 51.3)));
    }

    #[test]
    fn rejects_short_links_and_garbage() {
        assert_eq!(parse_location("https://maps.app.goo.gl/AbCdEf123"), None);
        assert_eq!(parse_location("https://goo.gl/maps/xyz"), None);
        assert_eq!(parse_location(""), None);
        assert_eq!(parse_location("hello world"), None);
        assert_eq!(parse_location("1,2,3"), None);
        assert_eq!(parse_location("https://example.com/page"), None);
        assert_eq!(parse_location("https://maps.google.com/?q=Tehran"), None);
    }

    #[test]
    fn rejects_out_of_range() {
        assert_eq!(parse_location("91, 10"), None);
        assert_eq!(parse_location("-90.5, 10"), None);
        assert_eq!(parse_location("10, 181"), None);
        assert_eq!(parse_location("10, -180.1"), None);
        assert_eq!(parse_location("https://www.google.com/maps/@95,10,15z"), None);
        assert_eq!(parse_location("NaN, 10"), None);
        assert_eq!(parse_location("90, 180"), Some((90.0, 180.0)));
    }

    #[test]
    fn maps_url_format() {
        assert_eq!(
            maps_url(35.6892, 51.389),
            "https://www.google.com/maps/search/?api=1&query=35.689200,51.389000"
        );
        assert_eq!(
            maps_url(-1.5, -0.1234567),
            "https://www.google.com/maps/search/?api=1&query=-1.500000,-0.123457"
        );
    }
}
