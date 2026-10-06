use std::sync::Arc;

use axum::{routing::get, Router};
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use crate::{
    config::Config,
    handlers::{
        handle_get_exchange_rate, handle_get_geo_near_allowed, handle_get_risk_report,
        handle_get_wallet_configs, handle_health,
    },
    services::{
        exchange_rate::ExchangeRateService, geo_service::GeoService,
        risk_checker::RiskCheckerService, wallet_config::WalletConfigService,
    },
};

#[derive(Clone)]
pub struct AppState {
    pub wallet_config_service: Arc<WalletConfigService>,
    pub exchange_rate_service: Arc<ExchangeRateService>,
    pub risk_checker_service: Arc<RiskCheckerService>,
    pub geo_service: Arc<GeoService>,
}

pub fn build_router(
    config: &Config,
    wallet_config_service: Arc<WalletConfigService>,
    exchange_rate_service: Arc<ExchangeRateService>,
    risk_checker_service: Arc<RiskCheckerService>,
    geo_service: Arc<GeoService>,
) -> Router {
    let state = AppState {
        wallet_config_service,
        exchange_rate_service,
        risk_checker_service,
        geo_service,
    };

    let origins = config.get_cors_allowed_origins();
    let cors = if origins.is_empty() {
        CorsLayer::new()
    } else {
        CorsLayer::new().allow_origin(origins)
    };

    Router::new()
        .route("/health", get(handle_health))
        .route("/api/configs/wallet", get(handle_get_wallet_configs))
        .route("/api/exchange-rates", get(handle_get_exchange_rate))
        .route("/api/geo/near-allowed", get(handle_get_geo_near_allowed))
        .route(
            "/api/risk-checker/{address_or_ens}",
            get(handle_get_risk_report),
        )
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    use uuid::Uuid;
    use wiremock::{
        matchers::{method, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    use crate::services::{
        exchange_rate::ExchangeRateSnapshot, watched_json::test_support::write_temp_json,
    };

    const GEO_CONFIG: &str = r#"{
  "nearAllowedCountries": ["SG"],
  "nearBlockedRanges": ["203.0.113.0/24"]
}"#;

    const VALID_FLAGS: &str = r#"{
  "enableTestButtons": false,
  "enableKeystoneHardwareWallet": false,
  "enableHighSecurity": true,
  "enableRemoteNotifications": true,
  "enableSwap": true
}"#;

    const MOCK_FUTURE_NEXT: i64 = 4_000_000_000;
    const TEST_ADDRESS: &str = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

    fn test_config() -> Config {
        Config {
            server: crate::config::ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                cors_allowed_origins: vec![],
            },
            logging: crate::config::LoggingConfig {
                level: "info".to_string(),
            },
            remote_configs: crate::config::RemoteConfigsConfig {
                wallet_configs_file: String::new(),
                geo_configs_file: String::new(),
            },
            exchange_rate: crate::config::ExchangeRateConfig {
                api_key: "test-key".to_string(),
            },
            risk_checker: crate::config::RiskCheckerConfig {
                etherscan_api_key: "test-key".to_string(),
                etherscan_base_url: "http://unused".to_string(),
                infura_api_key: "test-key".to_string(),
                infura_base_url: "http://unused".to_string(),
                etherscan_calls_per_sec: 3,
                max_concurrent_requests: 1,
            },
        }
    }

    fn cached_exchange_rate_service() -> Arc<ExchangeRateService> {
        let mut rates = HashMap::new();
        rates.insert("USD".to_string(), 1.0);
        rates.insert("EUR".to_string(), 0.9);
        Arc::new(ExchangeRateService::with_cached_snapshot(
            ExchangeRateSnapshot {
                conversion_rates: rates,
                time_next_update_unix: MOCK_FUTURE_NEXT,
            },
        ))
    }

    async fn setup_app() -> (Router, std::path::PathBuf) {
        setup_app_with_risk_checker(Arc::new(RiskCheckerService::new_with_base_url(
            "http://unused",
            "http://unused",
        )))
        .await
    }

    async fn setup_app_with_risk_checker(
        risk_checker_service: Arc<RiskCheckerService>,
    ) -> (Router, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("remote-config-http-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("flags.json");
        std::fs::write(&config_path, VALID_FLAGS).unwrap();

        let wallet_service = Arc::new(WalletConfigService::new(config_path).expect("service"));
        let geo_service =
            Arc::new(GeoService::new(write_temp_json("geo.json", GEO_CONFIG)).expect("geo"));
        let app = build_router(
            &test_config(),
            wallet_service,
            cached_exchange_rate_service(),
            risk_checker_service,
            geo_service,
        );
        (app, dir)
    }

    async fn get_json(
        app: Router,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, serde_json::Value) {
        let mut request = Request::builder().uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn get_wallet_configs_returns_data_envelope() {
        let (app, dir) = setup_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/configs/wallet")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["data"]["enableSwap"], true);
        assert_eq!(json["data"]["enableTestButtons"], false);

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn wallet_configs_carry_the_geo_verdict_for_the_caller() {
        let (app, dir) = setup_app().await;

        for (headers, allowed) in [
            (vec![("cf-ipcountry", "SG")], true),
            (vec![("cf-ipcountry", "GB")], false),
            (
                vec![("cf-ipcountry", "SG"), ("cf-connecting-ip", "203.0.113.4")],
                false,
            ),
            (vec![], false),
        ] {
            let (status, json) = get_json(app.clone(), "/api/configs/wallet", &headers).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(json["data"]["geoNearAllowed"], allowed, "{headers:?}");
            assert_eq!(json["data"]["enableSwap"], true);
        }

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn geo_near_allowed_shows_what_the_gate_saw() {
        let (app, dir) = setup_app().await;

        let (status, json) = get_json(
            app,
            "/api/geo/near-allowed",
            &[("cf-ipcountry", "gb"), ("cf-connecting-ip", "198.51.100.7")],
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["data"]["ip"], "198.51.100.7");
        assert_eq!(json["data"]["country"], "GB");
        assert_eq!(json["data"]["nearAllowed"], false);

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn get_exchange_rates_returns_data_envelope() {
        let (app, dir) = setup_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/exchange-rates")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["data"]["conversion_rates"]["USD"], 1.0);
        assert_eq!(json["data"]["conversion_rates"]["EUR"], 0.9);
        assert_eq!(json["data"]["time_next_update_unix"], MOCK_FUTURE_NEXT);

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn health_returns_ok_when_config_loaded() {
        let (app, dir) = setup_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn get_risk_report_rejects_invalid_input() {
        let (app, dir) = setup_app().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/risk-checker/not-valid-at-all")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "Invalid address or ENS name format");

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn get_risk_report_returns_data_envelope() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(query_param("module", "account"))
            .and(query_param("action", "balance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "1",
                "message": "OK",
                "result": "1000000000000000000"
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getTransactionCount"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "1",
                "message": "OK",
                "result": "0x0"
            })))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getCode"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "1",
                "message": "OK",
                "result": "0x"
            })))
            .mount(&mock_server)
            .await;

        let risk_checker_service = Arc::new(RiskCheckerService::new_with_base_url(
            &mock_server.uri(),
            "http://unused-infura",
        ));
        let (app, dir) = setup_app_with_risk_checker(risk_checker_service).await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/risk-checker/{TEST_ADDRESS}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["data"]["address"], TEST_ADDRESS);
        assert_eq!(json["data"]["balance"], "1000000000000000000");
        assert_eq!(json["data"]["balanceEth"], 1.0);
        assert_eq!(json["data"]["hasOutgoingTransactions"], false);
        assert_eq!(json["data"]["isSmartContract"], false);

        std::fs::remove_dir_all(dir).ok();
    }
}
