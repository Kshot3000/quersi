use axum::{
    extract::{Path, State},
    http::HeaderMap,
    response::IntoResponse,
    Json,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    error::AppError,
    services::{
        exchange_rate::ExchangeRateSnapshot,
        geo_service::GeoLookup,
        risk_checker::{RiskCheckerError, RiskCheckerService, RiskReport},
    },
    AppState,
};

/// Key the geo gate's verdict travels under in the wallet config payload.
pub const GEO_NEAR_ALLOWED_KEY: &str = "geoNearAllowed";

#[derive(Debug, Serialize)]
pub struct SuccessResponse<T> {
    pub data: T,
}

impl<T> SuccessResponse<T> {
    pub fn new(data: T) -> Self {
        Self { data }
    }
}

/// The flags file as is, plus the geo gate's verdict for this caller, so the
/// wallet learns both in the one request it already makes.
pub async fn handle_get_wallet_configs(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SuccessResponse<Value>>, AppError> {
    let mut flags = state.wallet_config_service.get_wallet_configs()?;
    let geo = state.geo_service.lookup(&headers).map_err(AppError::Geo)?;
    flags.insert(
        GEO_NEAR_ALLOWED_KEY.to_string(),
        Value::Bool(geo.near_allowed),
    );
    Ok(Json(SuccessResponse::new(Value::Object(flags))))
}

/// What the gate sees for the caller: their IP, country and verdict. For
/// checking a block list from the place it is meant to block.
pub async fn handle_get_geo_near_allowed(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SuccessResponse<GeoLookup>>, AppError> {
    let lookup = state.geo_service.lookup(&headers).map_err(AppError::Geo)?;
    Ok(Json(SuccessResponse::new(lookup)))
}

pub async fn handle_get_exchange_rate(
    State(state): State<AppState>,
) -> Result<Json<SuccessResponse<ExchangeRateSnapshot>>, AppError> {
    let snapshot = state.exchange_rate_service.get_snapshot().await?;
    Ok(Json(SuccessResponse::new(snapshot)))
}

pub async fn handle_get_risk_report(
    State(state): State<AppState>,
    Path(address_or_ens): Path<String>,
) -> Result<Json<SuccessResponse<RiskReport>>, AppError> {
    if !RiskCheckerService::is_valid_eth_address(&address_or_ens)
        && !RiskCheckerService::is_ens_name(&address_or_ens)
    {
        return Err(RiskCheckerError::InvalidInput.into());
    }

    let report = state
        .risk_checker_service
        .generate_report(&address_or_ens)
        .await?;

    Ok(Json(SuccessResponse::new(report)))
}

pub async fn handle_health() -> impl IntoResponse {
    Json(serde_json::json!({
        "healthy": true,
        "service": "RemoteConfig".to_string(),
        "version": env!("CARGO_PKG_VERSION").to_string(),
    }))
}
