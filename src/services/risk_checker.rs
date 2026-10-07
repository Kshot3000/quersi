use alloy::{ens::ProviderEnsExt, primitives::Address, providers::ProviderBuilder};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{str::FromStr, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::sync::Semaphore;

use crate::config::RiskCheckerConfig;

#[derive(Debug, Error)]
pub enum RiskCheckerError {
    #[error("Rate limit exceeded. Please try again later.")]
    RateLimit,
    #[error("The provided address could not be found on the Ethereum network. Please verify the address is correct.")]
    AddressNotFound,
    #[error("Unable to connect to blockchain data services. Please try again in a few moments.")]
    NetworkError,
    #[error("Invalid address or ENS name format")]
    InvalidInput,
    #[error("ENS name \"{0}\" could not be resolved to an Ethereum address")]
    EnsNotFound(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskReport {
    pub address: String,
    pub ens_name: Option<String>,
    pub original_input: String,
    pub balance: String,
    pub balance_eth: f64,
    pub has_outgoing_transactions: bool,
    pub first_transaction_timestamp: Option<i64>,
    pub days_since_first_transaction: Option<i64>,
    pub is_smart_contract: bool,
}

#[derive(Debug)]
pub enum AddressResolution {
    Resolved {
        address: String,
        ens_name: Option<String>,
    },
    Invalid,
    EnsNotFound,
}

#[derive(Debug, Deserialize)]
struct EtherscanResponse {
    status: String,
    result: serde_json::Value,
}

/// JSON-RPC envelope returned by Etherscan proxy endpoints.
/// Success: `{"jsonrpc":"2.0","id":1,"result":"0x5"}`
/// Error:   `{"jsonrpc":"2.0","id":1,"error":{"code":-32005,"message":"..."}}`
#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    result: Option<serde_json::Value>,
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct EtherscanTx {
    #[serde(rename = "timeStamp")]
    time_stamp: String,
}

#[derive(Debug)]
pub struct RiskCheckerService {
    client: Client,
    etherscan_api_key: String,
    etherscan_base_url: String,
    infura_rpc_url: String,
    /// Minimum delay between consecutive Etherscan calls to stay within the rate limit.
    etherscan_call_delay: Duration,
    /// Global semaphore that caps the number of concurrent `generate_report`
    /// executions across all inbound requests, preventing outbound Etherscan
    /// fan-out from overwhelming the API quota.
    concurrency_limiter: Arc<Semaphore>,
}

impl RiskCheckerService {
    pub fn new(config: &RiskCheckerConfig) -> Self {
        let infura_rpc_url = format!(
            "{}/{}",
            config.infura_base_url.trim_end_matches('/'),
            config.infura_api_key
        );
        let calls_per_sec = config.etherscan_calls_per_sec.max(1) as u64;
        let etherscan_call_delay = Duration::from_millis(1000 / calls_per_sec);

        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("TLS backend should be initialized, or the resolver should load the system configuration.");

        let max_concurrent = config.max_concurrent_requests.max(1);

        Self {
            client,
            etherscan_api_key: config.etherscan_api_key.clone(),
            etherscan_base_url: config.etherscan_base_url.clone(),
            infura_rpc_url,
            etherscan_call_delay,
            concurrency_limiter: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    pub fn is_valid_eth_address(input: &str) -> bool {
        let trimmed = input.trim();
        if !trimmed.starts_with("0x") || trimmed.len() != 42 {
            return false;
        }
        trimmed[2..].chars().all(|c| c.is_ascii_hexdigit())
    }

    pub fn is_ens_name(input: &str) -> bool {
        let trimmed = input.trim().to_lowercase();
        if !trimmed.ends_with(".eth") || trimmed.starts_with("0x") || trimmed.starts_with("-") {
            return false;
        }
        let label = &trimmed[..trimmed.len() - 4];
        !label.is_empty()
            && label
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '.')
    }

    pub async fn resolve_address_or_ens(
        &self,
        input: &str,
    ) -> Result<AddressResolution, RiskCheckerError> {
        let trimmed = input.trim();

        if Self::is_valid_eth_address(trimmed) {
            let ens_name = self.reverse_resolve_address(trimmed).await;
            return Ok(AddressResolution::Resolved {
                address: trimmed.to_lowercase(),
                ens_name,
            });
        }

        if Self::is_ens_name(trimmed) {
            match self.resolve_ens_name(trimmed).await {
                Some(address) => {
                    return Ok(AddressResolution::Resolved {
                        address,
                        ens_name: Some(trimmed.to_lowercase()),
                    });
                }
                None => return Ok(AddressResolution::EnsNotFound),
            }
        }

        Ok(AddressResolution::Invalid)
    }

    async fn resolve_ens_name(&self, ens_name: &str) -> Option<String> {
        let rpc_url = self.infura_rpc_url.parse().ok()?;
        let provider = ProviderBuilder::new().connect_http(rpc_url);
        match provider.resolve_name(ens_name).await {
            Ok(address) => Some(format!("{:#x}", address)),
            Err(e) => {
                tracing::warn!("Failed to resolve ENS name {}: {}", ens_name, e);
                None
            }
        }
    }

    async fn reverse_resolve_address(&self, address: &str) -> Option<String> {
        let rpc_url = self.infura_rpc_url.parse().ok()?;
        let provider = ProviderBuilder::new().connect_http(rpc_url);
        let addr = Address::from_str(address).ok()?;
        match provider.lookup_address(&addr).await {
            Ok(name) => Some(name),
            Err(e) => {
                tracing::debug!("No ENS reverse record for {}: {}", address, e);
                None
            }
        }
    }

    async fn fetch_etherscan(
        &self,
        params: &[(&str, &str)],
    ) -> Result<EtherscanResponse, RiskCheckerError> {
        let mut query: Vec<(&str, &str)> = params.to_vec();
        query.push(("apikey", &self.etherscan_api_key));

        let response = self
            .client
            .get(&self.etherscan_base_url)
            .query(&query)
            .send()
            .await
            .map_err(|e| {
                tracing::error!("Etherscan network error: {}", e);
                RiskCheckerError::NetworkError
            })?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(RiskCheckerError::RateLimit);
        }

        if !response.status().is_success() {
            return Err(RiskCheckerError::Other(format!(
                "HTTP {}: {}",
                response.status().as_u16(),
                response.status().canonical_reason().unwrap_or("Unknown")
            )));
        }

        let body = response.bytes().await.map_err(|e| {
            RiskCheckerError::Other(format!("Failed to read Etherscan response body: {}", e))
        })?;

        let value: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
            RiskCheckerError::Other(format!("Failed to parse Etherscan JSON: {}", e))
        })?;

        // Proxy endpoints (eth_getCode, eth_getTransactionCount, …) always use
        // a JSON-RPC envelope — they never have a top-level "status" field.
        // Detect by the presence of "jsonrpc" and normalise into EtherscanResponse.
        if value.get("jsonrpc").is_some() {
            let rpc: JsonRpcResponse = serde_json::from_value(value).map_err(|e| {
                RiskCheckerError::Other(format!("Failed to parse JSON-RPC response: {}", e))
            })?;

            if let Some(err) = rpc.error {
                let msg = err.message.to_lowercase();
                if msg.contains("rate limit") || msg.contains("max calls") {
                    return Err(RiskCheckerError::RateLimit);
                }
                return Err(RiskCheckerError::Other(err.message));
            }

            return Ok(EtherscanResponse {
                status: "1".to_string(),
                result: rpc.result.unwrap_or(serde_json::Value::Null),
            });
        }

        // Standard account-module response shape: {"status":"1","message":"OK","result":...}
        let data: EtherscanResponse = serde_json::from_value(value).map_err(|e| {
            RiskCheckerError::Other(format!("Failed to parse Etherscan response: {}", e))
        })?;

        if data.status == "0" {
            let result_str = data.result.as_str().unwrap_or("").to_lowercase();
            if result_str.contains("rate limit") || result_str.contains("max calls") {
                return Err(RiskCheckerError::RateLimit);
            }
            return Err(RiskCheckerError::AddressNotFound);
        }

        Ok(data)
    }

    pub async fn get_balance(&self, address: &str) -> Result<String, RiskCheckerError> {
        let data = self
            .fetch_etherscan(&[
                ("module", "account"),
                ("action", "balance"),
                ("address", address),
                ("tag", "latest"),
            ])
            .await?;

        data.result.as_str().map(|s| s.to_string()).ok_or_else(|| {
            RiskCheckerError::Other("Unexpected balance response format".to_string())
        })
    }

    pub async fn has_any_transactions(&self, address: &str) -> Result<bool, RiskCheckerError> {
        let data = self
            .fetch_etherscan(&[
                ("module", "proxy"),
                ("action", "eth_getTransactionCount"),
                ("address", address),
                ("tag", "latest"),
            ])
            .await?;

        let hex = data.result.as_str().ok_or_else(|| {
            RiskCheckerError::Other("Unexpected tx count response format".to_string())
        })?;

        let count = u64::from_str_radix(hex.trim_start_matches("0x"), 16)
            .map_err(|e| RiskCheckerError::Other(format!("Failed to parse tx count: {}", e)))?;

        Ok(count > 0)
    }

    pub async fn get_first_transaction_timestamp(
        &self,
        address: &str,
    ) -> Result<Option<i64>, RiskCheckerError> {
        let data = self
            .fetch_etherscan(&[
                ("module", "account"),
                ("action", "txlist"),
                ("address", address),
                ("startblock", "0"),
                ("endblock", "99999999"),
                ("page", "1"),
                ("offset", "1"),
                ("sort", "asc"),
            ])
            .await;

        // A "no transactions found" response comes back as status "0" which
        // fetch_etherscan maps to AddressNotFound — treat that as an empty result.
        let data = match data {
            Ok(d) => d,
            Err(RiskCheckerError::AddressNotFound) => return Ok(None),
            Err(e) => return Err(e),
        };

        let txs: Vec<EtherscanTx> = serde_json::from_value(data.result)
            .map_err(|e| RiskCheckerError::Other(format!("Failed to parse tx list: {}", e)))?;

        let timestamp = txs.first().and_then(|tx| tx.time_stamp.parse::<i64>().ok());
        Ok(timestamp)
    }

    pub async fn is_smart_contract(&self, address: &str) -> Result<bool, RiskCheckerError> {
        let data = self
            .fetch_etherscan(&[
                ("module", "proxy"),
                ("action", "eth_getCode"),
                ("address", address),
                ("tag", "latest"),
            ])
            .await?;

        let code = data.result.as_str().ok_or(RiskCheckerError::Other(
            "Unexpected code response format".to_string(),
        ))?;

        // eth_getCode returns "0x" when there is no code at the address.
        // Any deployed code at all means a contract, however small: an
        // EIP-1167 minimal proxy's runtime is only 45 bytes (92 chars with
        // the prefix), so a length threshold misreports proxies as EOAs.
        let hex = code.strip_prefix("0x").unwrap_or(code);
        Ok(!hex.is_empty())
    }

    pub fn wei_to_eth(wei: &str) -> f64 {
        wei.parse::<u128>().map(|w| w as f64 / 1e18).unwrap_or(0.0)
    }

    #[cfg(test)]
    pub fn new_with_base_url(etherscan_base_url: &str, infura_rpc_url: &str) -> Self {
        Self {
            client: Client::new(),
            etherscan_api_key: "test-key".to_string(),
            etherscan_base_url: etherscan_base_url.to_string(),
            infura_rpc_url: infura_rpc_url.to_string(),
            etherscan_call_delay: Duration::ZERO,
            concurrency_limiter: Arc::new(Semaphore::new(1)),
        }
    }

    pub async fn generate_report(&self, input: &str) -> Result<RiskReport, RiskCheckerError> {
        // Acquire a concurrency permit before touching Etherscan. If all permits
        // are taken the caller gets an immediate RateLimit error rather than
        // queuing indefinitely, which protects the outbound API quota.
        let _permit = self.concurrency_limiter.try_acquire().map_err(|_| {
            tracing::warn!("Risk checker concurrency limit reached; rejecting request");
            RiskCheckerError::RateLimit
        })?;

        let resolution = self.resolve_address_or_ens(input).await?;

        let (resolved_address, ens_name) = match resolution {
            AddressResolution::Invalid => return Err(RiskCheckerError::InvalidInput),
            AddressResolution::EnsNotFound => {
                return Err(RiskCheckerError::EnsNotFound(input.to_string()))
            }
            AddressResolution::Resolved { address, ens_name } => (address, ens_name),
        };

        let balance = self.get_balance(&resolved_address).await?;
        tokio::time::sleep(self.etherscan_call_delay).await;

        let has_outgoing_transactions = self.has_any_transactions(&resolved_address).await?;
        tokio::time::sleep(self.etherscan_call_delay).await;

        let is_smart_contract = self.is_smart_contract(&resolved_address).await?;
        tokio::time::sleep(self.etherscan_call_delay).await;

        let balance_eth = Self::wei_to_eth(&balance);

        let first_transaction_timestamp = if has_outgoing_transactions {
            let ts = self
                .get_first_transaction_timestamp(&resolved_address)
                .await?;
            tokio::time::sleep(self.etherscan_call_delay).await;
            ts
        } else {
            None
        };

        let days_since_first_transaction = first_transaction_timestamp.map(|ts| {
            let now = chrono::Utc::now().timestamp();
            let seconds = now - ts;
            seconds / (24 * 60 * 60)
        });

        Ok(RiskReport {
            address: resolved_address,
            ens_name,
            original_input: input.to_string(),
            balance,
            balance_eth,
            has_outgoing_transactions,
            first_transaction_timestamp,
            days_since_first_transaction,
            is_smart_contract,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        matchers::{method, query_param},
        Mock, MockServer, ResponseTemplate,
    };

    // -------------------------------------------------------------------------
    // Unit tests — pure functions, no I/O
    // -------------------------------------------------------------------------

    #[test]
    fn test_is_valid_eth_address_valid() {
        assert!(RiskCheckerService::is_valid_eth_address(
            "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"
        ));
    }

    #[test]
    fn test_is_valid_eth_address_lowercase() {
        assert!(RiskCheckerService::is_valid_eth_address(
            "0xd8da6bf26964af9d7eed9e03e53415d37aa96045"
        ));
    }

    #[test]
    fn test_is_valid_eth_address_too_short() {
        assert!(!RiskCheckerService::is_valid_eth_address("0xabc123"));
    }

    #[test]
    fn test_is_valid_eth_address_missing_prefix() {
        assert!(!RiskCheckerService::is_valid_eth_address(
            "d8da6bf26964af9d7eed9e03e53415d37aa96045"
        ));
    }

    #[test]
    fn test_is_valid_eth_address_invalid_chars() {
        // 'z' is not a hex digit
        assert!(!RiskCheckerService::is_valid_eth_address(
            "0xzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
        ));
    }

    #[test]
    fn test_is_valid_eth_address_empty() {
        assert!(!RiskCheckerService::is_valid_eth_address(""));
    }

    #[test]
    fn test_is_ens_name_valid() {
        assert!(RiskCheckerService::is_ens_name("vitalik.eth"));
    }

    #[test]
    fn test_is_ens_name_with_sub_valid() {
        assert!(RiskCheckerService::is_ens_name("pay.vitalik.eth"));
    }

    #[test]
    fn test_is_ens_name_with_numbers() {
        assert!(RiskCheckerService::is_ens_name("wallet123.eth"));
    }

    #[test]
    fn test_is_ens_name_with_hyphen() {
        assert!(RiskCheckerService::is_ens_name("my-wallet.eth"));
    }

    #[test]
    fn test_is_ens_name_uppercase_normalised() {
        assert!(RiskCheckerService::is_ens_name("Vitalik.ETH"));
    }

    #[test]
    fn test_is_ens_name_wrong_tld() {
        assert!(!RiskCheckerService::is_ens_name("vitalik.com"));
    }

    #[test]
    fn test_is_ens_name_eth_address_rejected() {
        // Starts with 0x — must not be treated as ENS
        assert!(!RiskCheckerService::is_ens_name(
            "0xd8da6bf26964af9d7eed9e03e53415d37aa96045.eth"
        ));
    }

    #[test]
    fn test_is_ens_name_starts_with_hyphen_rejected() {
        // Starts with 0x — must not be treated as ENS
        assert!(!RiskCheckerService::is_ens_name("-alice.eth"));
    }

    #[test]
    fn test_is_ens_name_empty_label() {
        assert!(!RiskCheckerService::is_ens_name(".eth"));
    }

    #[test]
    fn test_is_ens_name_empty_string() {
        assert!(!RiskCheckerService::is_ens_name(""));
    }

    #[test]
    fn test_wei_to_eth_one_eth() {
        let result = RiskCheckerService::wei_to_eth("1000000000000000000");
        assert!((result - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_wei_to_eth_zero() {
        assert_eq!(RiskCheckerService::wei_to_eth("0"), 0.0);
    }

    #[test]
    fn test_wei_to_eth_fractional() {
        // 0.5 ETH = 500_000_000_000_000_000 wei
        let result = RiskCheckerService::wei_to_eth("500000000000000000");
        assert!((result - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_wei_to_eth_invalid_string() {
        assert_eq!(RiskCheckerService::wei_to_eth("not-a-number"), 0.0);
    }

    // -------------------------------------------------------------------------
    // Integration tests — Etherscan HTTP calls mocked with wiremock
    // -------------------------------------------------------------------------

    fn etherscan_ok(result: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "status": "1", "message": "OK", "result": result })
    }

    fn etherscan_notok() -> serde_json::Value {
        serde_json::json!({ "status": "0", "message": "NOTOK", "result": "" })
    }

    async fn setup_service(mock_server: &MockServer) -> RiskCheckerService {
        RiskCheckerService::new_with_base_url(&mock_server.uri(), "http://unused-infura")
    }

    #[tokio::test]
    async fn test_get_balance_success() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        Mock::given(method("GET"))
            .and(query_param("module", "account"))
            .and(query_param("action", "balance"))
            .and(query_param("address", address))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(etherscan_ok(serde_json::json!("1000000000000000000"))),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.get_balance(address).await;

        // Assert
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "1000000000000000000");
    }

    #[tokio::test]
    async fn test_get_balance_address_not_found() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        Mock::given(method("GET"))
            .and(query_param("module", "account"))
            .and(query_param("action", "balance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(etherscan_notok()))
            .expect(1)
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.get_balance(address).await;

        // Assert
        assert!(matches!(result, Err(RiskCheckerError::AddressNotFound)));
    }

    #[tokio::test]
    async fn test_get_balance_rate_limit() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        Mock::given(method("GET"))
            .and(query_param("module", "account"))
            .and(query_param("action", "balance"))
            .respond_with(ResponseTemplate::new(429))
            .expect(1)
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.get_balance(address).await;

        // Assert
        assert!(matches!(result, Err(RiskCheckerError::RateLimit)));
    }

    #[tokio::test]
    async fn test_has_any_transactions_true() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getTransactionCount"))
            .and(query_param("address", address))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(etherscan_ok(serde_json::json!("0x5"))),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.has_any_transactions(address).await;

        // Assert
        assert!(result.is_ok());
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn test_has_any_transactions_false() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getTransactionCount"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(etherscan_ok(serde_json::json!("0x0"))),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.has_any_transactions(address).await;

        // Assert
        assert!(result.is_ok());
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn test_is_smart_contract_true() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";
        // A code string longer than 100 chars indicates a contract
        let bytecode = "0x".to_string() + &"60".repeat(60);

        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getCode"))
            .and(query_param("address", address))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(etherscan_ok(serde_json::json!(bytecode))),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.is_smart_contract(address).await.unwrap();

        // Assert
        assert!(result);
    }

    #[tokio::test]
    async fn test_is_smart_contract_true_minimal_proxy() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";
        // An EIP-1167 minimal proxy has a 45-byte runtime (92 chars with the
        // "0x" prefix) — small, but it is deployed contract code, not an EOA.
        let bytecode = format!(
            "0x363d3d373d3d3d363d73{}5af43d82803e903d91602b57fd5bf3",
            "be".repeat(20)
        );
        assert_eq!(bytecode.len(), 92);

        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getCode"))
            .and(query_param("address", address))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(etherscan_ok(serde_json::json!(bytecode))),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.is_smart_contract(address).await.unwrap();

        // Assert
        assert!(result);
    }

    #[tokio::test]
    async fn test_is_smart_contract_false_eoa() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getCode"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(etherscan_ok(serde_json::json!("0x"))),
            )
            .expect(1)
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.is_smart_contract(address).await.unwrap();

        // Assert
        assert!(!result);
    }

    #[tokio::test]
    async fn test_get_first_transaction_timestamp_success() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";
        let expected_ts: i64 = 1438918233;

        Mock::given(method("GET"))
            .and(query_param("module", "account"))
            .and(query_param("action", "txlist"))
            .and(query_param("address", address))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "1",
                "message": "OK",
                "result": [{ "timeStamp": expected_ts.to_string() }]
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.get_first_transaction_timestamp(address).await;

        // Assert
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Some(expected_ts));
    }

    #[tokio::test]
    async fn test_get_first_transaction_timestamp_no_transactions() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        // Etherscan returns status "0" / NOTOK when there are no transactions —
        // our code treats this as an empty result, not an error.
        Mock::given(method("GET"))
            .and(query_param("module", "account"))
            .and(query_param("action", "txlist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(etherscan_notok()))
            .expect(1)
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.get_first_transaction_timestamp(address).await;

        // Assert
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn test_generate_report_for_eoa_with_transactions() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        // balance
        Mock::given(method("GET"))
            .and(query_param("module", "account"))
            .and(query_param("action", "balance"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(etherscan_ok(serde_json::json!("2000000000000000000"))),
            )
            .mount(&mock_server)
            .await;

        // tx count — has transactions
        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getTransactionCount"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(etherscan_ok(serde_json::json!("0xa"))),
            )
            .mount(&mock_server)
            .await;

        // eth_getCode — EOA (short result)
        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getCode"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(etherscan_ok(serde_json::json!("0x"))),
            )
            .mount(&mock_server)
            .await;

        // first tx timestamp
        Mock::given(method("GET"))
            .and(query_param("module", "account"))
            .and(query_param("action", "txlist"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "1",
                "message": "OK",
                "result": [{ "timeStamp": "1438918233" }]
            })))
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.generate_report(address).await;

        // Assert
        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.address, address.to_lowercase());
        assert_eq!(report.original_input, address);
        assert_eq!(report.balance, "2000000000000000000");
        assert!((report.balance_eth - 2.0).abs() < 1e-9);
        assert!(report.has_outgoing_transactions);
        assert_eq!(report.first_transaction_timestamp, Some(1438918233));
        assert!(report.days_since_first_transaction.is_some());
        assert!(!report.is_smart_contract);
        assert!(report.ens_name.is_none());
    }

    #[tokio::test]
    async fn test_generate_report_for_address_with_no_transactions() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        Mock::given(method("GET"))
            .and(query_param("module", "account"))
            .and(query_param("action", "balance"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(etherscan_ok(serde_json::json!("0"))),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getTransactionCount"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(etherscan_ok(serde_json::json!("0x0"))),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(query_param("module", "proxy"))
            .and(query_param("action", "eth_getCode"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(etherscan_ok(serde_json::json!("0x"))),
            )
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.generate_report(address).await;

        // Assert
        assert!(result.is_ok());
        let report = result.unwrap();
        assert!(!report.has_outgoing_transactions);
        assert_eq!(report.first_transaction_timestamp, None);
        assert_eq!(report.days_since_first_transaction, None);
    }

    #[tokio::test]
    async fn test_generate_report_invalid_input_returns_error() {
        // Arrange — no mocks needed; validation is synchronous
        let mock_server = MockServer::start().await;
        let service = setup_service(&mock_server).await;

        // Act
        let result = service.generate_report("not-valid-at-all").await;

        // Assert
        assert!(matches!(result, Err(RiskCheckerError::InvalidInput)));
    }

    #[tokio::test]
    async fn test_generate_report_propagates_rate_limit_error() {
        // Arrange
        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&mock_server)
            .await;

        let service = setup_service(&mock_server).await;

        // Act
        let result = service.generate_report(address).await;

        // Assert
        assert!(matches!(result, Err(RiskCheckerError::RateLimit)));
    }

    /// Verify that requests beyond the concurrency limit are immediately
    /// rejected with RateLimit rather than queuing and hammering Etherscan.
    #[tokio::test]
    async fn test_generate_report_concurrency_limit_rejects_excess_requests() {
        use std::sync::Arc;
        use tokio::sync::Barrier;

        let mock_server = MockServer::start().await;
        let address = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

        // Respond slowly so the first request holds the permit long enough for
        // the second to arrive and be rejected.
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(200))
                    .set_body_json(
                        serde_json::json!({ "status": "0", "message": "NOTOK", "result": "" }),
                    ),
            )
            .mount(&mock_server)
            .await;

        // Limit to exactly 1 concurrent request.
        let service = Arc::new(setup_service(&mock_server).await);

        // Use a barrier so both tasks start at the same instant.
        let barrier = Arc::new(Barrier::new(2));

        let svc1 = service.clone();
        let b1 = barrier.clone();
        let addr = address.to_string();
        let t1 = tokio::spawn(async move {
            b1.wait().await;
            svc1.generate_report(&addr).await
        });

        let svc2 = service.clone();
        let b2 = barrier.clone();
        let addr = address.to_string();
        let t2 = tokio::spawn(async move {
            b2.wait().await;
            svc2.generate_report(&addr).await
        });

        let (r1, r2) = tokio::join!(t1, t2);
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();

        // Exactly one of the two concurrent calls must be rejected with RateLimit.
        let rate_limited = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Err(RiskCheckerError::RateLimit)))
            .count();

        assert_eq!(
            rate_limited, 1,
            "expected exactly one request to be rate-limited; got r1={r1:?}, r2={r2:?}"
        );
    }
}
