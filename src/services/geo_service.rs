use std::{net::IpAddr, path::PathBuf};

use axum::http::HeaderMap;
use ipnet::IpNet;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::watched_json::{WatchedJson, WatchedJsonError};

pub type GeoError = WatchedJsonError;

/// An ISO 3166-1 alpha-2 code as Cloudflare reports it: two ASCII letters,
/// kept upper case. Whitespace and case in a config entry or a header are
/// normalized away; anything else is refused, so a typo in the allow list
/// fails the load instead of silently matching nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountryCode(String);

impl CountryCode {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let code: String = raw
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase()) {
            Ok(Self(code))
        } else {
            Err(format!("{raw:?} is not a two-letter country code"))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for CountryCode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl Serialize for CountryCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Where NEAR Intents may be used: in the listed countries only, and never
/// from a blocked range. Hot-reloaded from a JSON file, so adding a country or
/// a range is an edit on the host, not a deploy. An unknown key, a malformed
/// country or an unparsable range is rejected so a typo cannot silently move
/// the gate.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GeoConfig {
    /// The only countries NEAR is open to, as Cloudflare reports them in
    /// CF-IPCountry.
    pub near_allowed_countries: Vec<CountryCode>,
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
    pub country: Option<CountryCode>,
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
    /// Cloudflare's code for a country it cannot tell. Its Tor code, T1, is not
    /// two letters and so already parses as unknown.
    const UNKNOWN_COUNTRY: &'static str = "XX";
    /// Every header a proxy chain reports a client address in, Cloudflare's
    /// first. With Cloudflare's Pseudo IPv4 on, CF-Connecting-IP holds a
    /// synthetic IPv4 and the real address is in CF-Connecting-IPv6, so block
    /// ranges are checked against all of them and the first is reported.
    const IP_HEADERS: [&'static str; 4] = [
        "cf-connecting-ipv6",
        "cf-connecting-ip",
        "x-forwarded-for",
        "x-real-ip",
    ];

    pub fn new(file_path: impl Into<PathBuf>) -> Result<Self, GeoError> {
        Ok(Self {
            config: WatchedJson::new(file_path)?,
        })
    }

    pub fn lookup(&self, headers: &HeaderMap) -> Result<GeoLookup, GeoError> {
        let ips = Self::client_ips(headers);
        let country = Self::country(headers);
        let near_allowed = self
            .config
            .with(|config| Self::near_allowed(config, &ips, country.as_ref()))?;
        Ok(GeoLookup {
            ip: ips.first().copied(),
            country,
            near_allowed,
        })
    }

    /// A blocked range on any reported address wins over any country; then the
    /// country decides, and no country at all falls back to `nearAllowedWhenUnknown`.
    fn near_allowed(config: &GeoConfig, ips: &[IpAddr], country: Option<&CountryCode>) -> bool {
        if ips.iter().any(|ip| {
            config
                .near_blocked_ranges
                .iter()
                .any(|range| range.contains(ip))
        }) {
            return false;
        }
        match country {
            None => config.near_allowed_when_unknown,
            Some(code) => config.near_allowed_countries.contains(code),
        }
    }

    fn country(headers: &HeaderMap) -> Option<CountryCode> {
        let code = CountryCode::parse(headers.get(Self::COUNTRY_HEADER)?.to_str().ok()?).ok()?;
        (code.as_str() != Self::UNKNOWN_COUNTRY).then_some(code)
    }

    /// The first address of each known header; the rest of a forwarding chain
    /// are proxies.
    fn client_ips(headers: &HeaderMap) -> Vec<IpAddr> {
        Self::IP_HEADERS
            .iter()
            .filter_map(|name| {
                let value = headers.get(*name)?.to_str().ok()?;
                value.split(',').next()?.trim().parse().ok()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::watched_json::test_support::write_temp_json;
    use axum::http::HeaderValue;
    use std::time::Duration;

    const CONFIG: &str = r#"{
  "nearAllowedCountries": ["SG", " de "],
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

    fn country(lookup: &GeoLookup) -> Option<&str> {
        lookup.country.as_ref().map(CountryCode::as_str)
    }

    #[tokio::test]
    async fn allows_only_listed_countries_whatever_their_case_or_spacing() {
        let service = service(CONFIG);
        for (code, allowed) in [
            ("SG", true),
            ("sg", true),
            (" Sg ", true),
            ("DE", true),
            ("GB", false),
            ("US", false),
        ] {
            let lookup = service.lookup(&headers(&[("cf-ipcountry", code)])).unwrap();
            assert_eq!(lookup.near_allowed, allowed, "{code}");
            assert_eq!(
                country(&lookup),
                Some(code.trim().to_ascii_uppercase().as_str())
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
            vec![("cf-ipcountry", "RUS")],
            vec![("cf-ipcountry", "")],
        ] {
            let lookup = closed.lookup(&headers(&pairs)).unwrap();
            assert_eq!(country(&lookup), None, "{pairs:?}");
            assert!(!lookup.near_allowed, "{pairs:?}");
        }

        let open = service(r#"{"nearAllowedCountries": ["SG"], "nearAllowedWhenUnknown": true}"#);
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
    async fn checks_the_real_ipv6_behind_cloudflares_pseudo_ipv4() {
        let service = service(CONFIG);
        let lookup = service
            .lookup(&headers(&[
                ("cf-ipcountry", "DE"),
                ("cf-connecting-ip", "240.16.0.1"),
                ("cf-connecting-ipv6", "2001:db8::1"),
            ]))
            .unwrap();
        assert_eq!(lookup.ip, Some("2001:db8::1".parse().unwrap()));
        assert!(!lookup.near_allowed);

        let blocked_v4_behind_allowed_v6 = service
            .lookup(&headers(&[
                ("cf-ipcountry", "DE"),
                ("cf-connecting-ip", "203.0.113.1"),
                ("cf-connecting-ipv6", "2001:4860::1"),
            ]))
            .unwrap();
        assert!(!blocked_v4_behind_allowed_v6.near_allowed);
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
    async fn rejects_a_bad_country_a_bad_range_or_an_unknown_key_at_startup() {
        for config in [
            r#"{"nearAllowedCountries": ["SGP"]}"#,
            r#"{"nearAllowedCountries": ["S1"]}"#,
            r#"{"nearAllowedCountries": [""]}"#,
            r#"{"nearAllowedCountries": [], "nearBlockedRanges": ["203.0.113.0/99"]}"#,
            r#"{"nearAllowedCountries": [], "nearAllowedCountry": ["SG"]}"#,
            r#"{"nearBlockedCountries": ["RU"]}"#,
        ] {
            let err = GeoService::new(write_temp_json("geo.json", config)).expect_err(config);
            assert!(matches!(err, GeoError::ParseJson(_)), "{config}");
        }
    }

    #[tokio::test]
    async fn a_reload_with_a_bad_country_keeps_the_last_policy_and_a_good_one_is_normalized() {
        let path = write_temp_json("geo.json", r#"{"nearAllowedCountries": ["SG"]}"#);
        let service = GeoService::new(path.clone()).unwrap();
        let sg = headers(&[("cf-ipcountry", "SG")]);
        let de = headers(&[("cf-ipcountry", "DE")]);
        assert!(service.lookup(&sg).unwrap().near_allowed);

        std::fs::write(&path, r#"{"nearAllowedCountries": ["SGP"]}"#).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(service.lookup(&sg).unwrap().near_allowed);

        std::fs::write(&path, r#"{"nearAllowedCountries": [" de "]}"#).unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !service.lookup(&de).unwrap().near_allowed {
            assert!(
                tokio::time::Instant::now() < deadline,
                "reload not applied within 2s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!service.lookup(&sg).unwrap().near_allowed);
    }
}
