use std::{net::IpAddr, path::PathBuf};

use axum::http::HeaderMap;
use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use super::watched_json::{WatchedJson, WatchedJsonError};

pub type GeoError = WatchedJsonError;

/// Where NEAR Intents may not be used. Hot-reloaded from a JSON file, so
/// adding a country or a range is an edit on the host, not a deploy. An
/// unknown key or an unparsable range is rejected so a typo cannot silently
/// open the gate.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GeoConfig {
    /// ISO 3166-1 alpha-2 codes, as Cloudflare reports them in CF-IPCountry.
    pub near_blocked_countries: Vec<String>,
    /// CIDR ranges blocked whatever country they resolve to.
    #[serde(default)]
    pub near_blocked_ranges: Vec<IpNet>,
    /// Whether a request whose country cannot be told may use NEAR. Off by
    /// default: a missing Cloudflare header closes the gate, it does not open it.
    #[serde(default)]
    pub near_allowed_when_unknown: bool,
}

/// What the gate saw for one request and what it decided.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeoLookup {
    pub ip: Option<IpAddr>,
    pub country: Option<String>,
    pub near_allowed: bool,
}

/// Tells, per request, whether the caller is somewhere NEAR Intents is
/// available. The service sits behind Cloudflare, which stamps every proxied
/// request with the visitor's country and IP; nothing else is trusted for it.
#[derive(Debug)]
pub struct GeoService {
    config: WatchedJson<GeoConfig>,
}

impl GeoService {
    const COUNTRY_HEADER: &'static str = "cf-ipcountry";
    const IP_HEADERS: [&'static str; 3] = ["cf-connecting-ip", "x-forwarded-for", "x-real-ip"];
    /// Cloudflare's codes for "unknown" and "Tor exit node".
    const UNKNOWN_COUNTRIES: [&'static str; 2] = ["XX", "T1"];

    pub fn new(file_path: impl Into<PathBuf>) -> Result<Self, GeoError> {
        Ok(Self {
            config: WatchedJson::new(file_path)?,
        })
    }

    pub fn lookup(&self, headers: &HeaderMap) -> Result<GeoLookup, GeoError> {
        let ip = Self::client_ip(headers);
        let country = Self::country(headers);
        let near_allowed = self
            .config
            .with(|config| Self::near_allowed(config, ip, country.as_deref()))?;
        Ok(GeoLookup {
            ip,
            country,
            near_allowed,
        })
    }

    /// A blocked range wins over any country; then the country decides, and
    /// no country at all falls back to `nearAllowedWhenUnknown`.
    fn near_allowed(config: &GeoConfig, ip: Option<IpAddr>, country: Option<&str>) -> bool {
        if let Some(ip) = ip {
            if config
                .near_blocked_ranges
                .iter()
                .any(|range| range.contains(&ip))
            {
                return false;
            }
        }
        match country {
            None => config.near_allowed_when_unknown,
            Some(code) => !config
                .near_blocked_countries
                .iter()
                .any(|blocked| blocked.eq_ignore_ascii_case(code)),
        }
    }

    fn country(headers: &HeaderMap) -> Option<String> {
        let code = headers
            .get(Self::COUNTRY_HEADER)?
            .to_str()
            .ok()?
            .trim()
            .to_ascii_uppercase();
        if code.is_empty() || Self::UNKNOWN_COUNTRIES.contains(&code.as_str()) {
            return None;
        }
        Some(code)
    }

    /// The first address of the first known header: Cloudflare's own, then
    /// the forwarding headers a plain reverse proxy sets.
    fn client_ip(headers: &HeaderMap) -> Option<IpAddr> {
        Self::IP_HEADERS.iter().find_map(|name| {
            let value = headers.get(*name)?.to_str().ok()?;
            value.split(',').next()?.trim().parse().ok()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::watched_json::test_support::write_temp_json;
    use axum::http::HeaderValue;

    const CONFIG: &str = r#"{
  "nearBlockedCountries": ["CN", "gb"],
  "nearBlockedRanges": ["203.0.113.0/24", "2001:db8::/32"]
}"#;

    fn service(config: &str) -> GeoService {
        GeoService::new(write_temp_json("geo.json", config)).expect("geo service")
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[tokio::test]
    async fn blocks_listed_countries_whatever_their_case() {
        let service = service(CONFIG);
        for (code, allowed) in [
            ("GB", false),
            ("gb", false),
            ("CN", false),
            ("DE", true),
            ("US", true),
        ] {
            let lookup = service.lookup(&headers(&[("cf-ipcountry", code)])).unwrap();
            assert_eq!(lookup.near_allowed, allowed, "{code}");
            assert_eq!(
                lookup.country.as_deref(),
                Some(code.to_ascii_uppercase().as_str())
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_country_is_closed_unless_configured_open() {
        let closed = service(CONFIG);
        for pairs in [
            vec![],
            vec![("cf-ipcountry", "XX")],
            vec![("cf-ipcountry", "T1")],
        ] {
            let lookup = closed.lookup(&headers(&pairs)).unwrap();
            assert_eq!(lookup.country, None);
            assert!(!lookup.near_allowed);
        }

        let open = service(r#"{"nearBlockedCountries": ["CN"], "nearAllowedWhenUnknown": true}"#);
        assert!(open.lookup(&headers(&[])).unwrap().near_allowed);
        assert!(
            !open
                .lookup(&headers(&[("cf-ipcountry", "CN")]))
                .unwrap()
                .near_allowed
        );
    }

    #[tokio::test]
    async fn a_blocked_range_wins_over_an_allowed_country() {
        let service = service(CONFIG);
        let lookup = service
            .lookup(&headers(&[
                ("cf-ipcountry", "DE"),
                ("cf-connecting-ip", "203.0.113.9"),
            ]))
            .unwrap();
        assert_eq!(lookup.ip, Some("203.0.113.9".parse().unwrap()));
        assert!(!lookup.near_allowed);

        let v6 = service
            .lookup(&headers(&[
                ("cf-ipcountry", "DE"),
                ("x-real-ip", "2001:db8::1"),
            ]))
            .unwrap();
        assert!(!v6.near_allowed);

        let outside = service
            .lookup(&headers(&[
                ("cf-ipcountry", "DE"),
                ("cf-connecting-ip", "198.51.100.1"),
            ]))
            .unwrap();
        assert!(outside.near_allowed);
    }

    #[tokio::test]
    async fn takes_the_first_forwarded_address_and_ignores_garbage() {
        let service = service(CONFIG);
        let lookup = service
            .lookup(&headers(&[
                ("cf-ipcountry", "DE"),
                ("x-forwarded-for", "203.0.113.2, 10.0.0.1"),
            ]))
            .unwrap();
        assert_eq!(lookup.ip, Some("203.0.113.2".parse().unwrap()));
        assert!(!lookup.near_allowed);

        let garbage = service
            .lookup(&headers(&[
                ("cf-ipcountry", "DE"),
                ("x-forwarded-for", "not-an-ip"),
            ]))
            .unwrap();
        assert_eq!(garbage.ip, None);
        assert!(garbage.near_allowed);
    }

    #[tokio::test]
    async fn rejects_a_bad_range_or_an_unknown_key_at_startup() {
        for config in [
            r#"{"nearBlockedCountries": [], "nearBlockedRanges": ["203.0.113.0/99"]}"#,
            r#"{"nearBlockedCountries": [], "nearBlockedCountry": ["CN"]}"#,
        ] {
            let err = GeoService::new(write_temp_json("geo.json", config)).expect_err(config);
            assert!(matches!(err, GeoError::ParseJson(_)), "{config}");
        }
    }
}
