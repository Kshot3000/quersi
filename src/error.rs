use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use thiserror::Error;

use crate::services::{
    exchange_rate::ExchangeRateError, geo_service::GeoError, risk_checker::RiskCheckerError,
    wallet_config::WalletConfigsError,
};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration error: {0}")]
    Config(#[from] config::ConfigError),
    #[error("config path has no parent directory")]
    MissingConfigParent,
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    WalletConfigs(#[from] WalletConfigsError),
    #[error(transparent)]
    ExchangeRate(#[from] ExchangeRateError),
    #[error(transparent)]
    RiskChecker(#[from] RiskCheckerError),
    #[error(transparent)]
    Geo(GeoError),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            AppError::WalletConfigs(WalletConfigsError::ReadLock(_)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read wallet configs".to_string(),
            ),
            AppError::WalletConfigs(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "wallet configs unavailable".to_string(),
            ),
            AppError::ExchangeRate(err) => map_exchange_rate_error(err),
            AppError::RiskChecker(err) => map_risk_checker_error(err),
            AppError::Geo(err) => {
                tracing::error!("Geo lookup error: {err}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "geo lookup unavailable".to_string(),
                )
            }
        };

        (status, Json(json!({ "error": message }))).into_response()
    }
}

fn map_exchange_rate_error(err: &ExchangeRateError) -> (StatusCode, String) {
    match err {
        ExchangeRateError::Api(detail) => {
            tracing::error!("Exchange rate API error: {detail}");
            (
                StatusCode::BAD_GATEWAY,
                "Failed to fetch exchange rates".to_string(),
            )
        }
        ExchangeRateError::Http(e) => {
            tracing::error!("Exchange rate HTTP error: {e}");
            (
                StatusCode::BAD_GATEWAY,
                "Failed to fetch exchange rates".to_string(),
            )
        }
        ExchangeRateError::Json(e) => {
            tracing::error!("Exchange rate JSON parse error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to parse exchange rate response".to_string(),
            )
        }
        ExchangeRateError::Cache(detail) => {
            tracing::error!("Exchange rate cache error: {detail}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "An internal server error occurred".to_string(),
            )
        }
    }
}

fn map_risk_checker_error(err: &RiskCheckerError) -> (StatusCode, String) {
    match err {
        RiskCheckerError::InvalidInput => (StatusCode::BAD_REQUEST, err.to_string()),
        RiskCheckerError::EnsNotFound(name) => (
            StatusCode::NOT_FOUND,
            format!(
                "The ENS name \"{}\" could not be resolved to an Ethereum address. Please verify the .eth name is correct.",
                name
            ),
        ),
        RiskCheckerError::AddressNotFound => (StatusCode::NOT_FOUND, err.to_string()),
        RiskCheckerError::RateLimit => (StatusCode::TOO_MANY_REQUESTS, err.to_string()),
        RiskCheckerError::NetworkError => (StatusCode::SERVICE_UNAVAILABLE, err.to_string()),
        RiskCheckerError::Other(msg) => {
            tracing::error!("Risk checker error: {msg}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "An internal server error occurred".to_string(),
            )
        }
    }
}
