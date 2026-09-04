# Quersi (QRC) - Quantus Remote Config

Minimal Axum service that serves wallet feature flags and network endpoint overrides, USD exchange rates, and Ethereum address risk reports over HTTP.

## Design

Feature flags are intentionally **public to read**. There is **no write/admin HTTP API** — updates are done by replacing the JSON file on disk (hot-reloaded).

Signing config files does not help against a compromised host: anyone who can replace the service binary (or its config) can serve arbitrary responses anyway. Trust is host/deploy integrity, not a signature on the JSON.

```text
Operator/CI --> wallet_config.json --> VPS mount
                                    |
                          remote-config (load / hot-reload)
                                    |
                          GET /api/configs/wallet

ExchangeRate-API v6  -->  in-memory cache  -->  GET /api/exchange-rates

Infura (ENS) + Etherscan  -->  GET /api/risk-checker/{address_or_ens}
```

## API

| Method | Path | Auth | Response |
|--------|------|------|----------|
| `GET` | `/api/configs/wallet` | none | `{ "data": { ...flags } }` |
| `GET` | `/api/exchange-rates` | none | `{ "data": { "conversion_rates": { ... }, "time_next_update_unix": … } }` |
| `GET` | `/api/risk-checker/{address_or_ens}` | none | `{ "data": { "address", "ensName", "balance", … } }` |
| `GET` | `/health` | none | `{ "healthy": true, "service": "RemoteConfig", "version": "…" }` |

Example wallet payload:

```json
{
  "data": {
    "enableTestButtons": false,
    "enableKeystoneHardwareWallet": false,
    "enableHighSecurity": true,
    "enableRemoteNotifications": true,
    "enableSwap": true,
    "endpoints": {
      "rpc": ["https://a1-planck.quantus.cat", "https://a2-planck.quantus.cat"],
      "graphQl": ["https://sub2.quantus.com/v1/graphql"],
      "explorer": "https://explorer.quantus.com",
      "senoti": "https://snt.quantus.com/api"
    }
  }
}
```

Flags are opaque JSON: add or change keys by updating the file — no service redeploy required.

### Network endpoints

The optional `endpoints` object moves wallets to another network without an app release. Every key is optional; a wallet keeps its built-in default for any key that is absent.

| Key | Type | Used for |
|-----|------|----------|
| `rpc` | non-empty array of `http(s)`/`ws(s)` URLs | chain RPC nodes (the wallet picks the fastest and fails over) |
| `graphQl` | non-empty array of `http(s)` URLs | Subsquid/Hasura indexer |
| `explorer` | `http(s)` URL | block-explorer links |
| `senoti` | `http(s)` URL | push-notification service |

Because a malformed URL here would strand every wallet on an unreachable network, the `endpoints` block is validated when the file is loaded: a present key with the wrong shape is rejected like invalid JSON (startup fails; a hot reload keeps the last known good file and logs a warning). Everything outside `endpoints` is still passed through untouched.

Exchange rates are fetched from [ExchangeRate-API](https://www.exchangerate-api.com/) (v6, USD base) and cached in memory until `time_next_update_unix`.

Risk reports resolve an Ethereum address or `.eth` ENS name (via Infura) and fetch balance / transaction / contract data from Etherscan. Invalid input returns `400`; unresolved ENS or missing address data returns `404`; upstream rate limits return `429`.

CORS is off by default (`cors_allowed_origins = []`). Set origins in TOML when browser clients need cross-origin access.

## Quick start

`wallet_config.json` is gitignored (host-managed). Copy the example first:

```bash
cp wallet_config.example.json wallet_config.json
cargo run -- --config config/default.toml
# GET http://127.0.0.1:6767/api/configs/wallet
# GET http://127.0.0.1:6767/api/exchange-rates
# GET http://127.0.0.1:6767/api/risk-checker/0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045
# GET http://127.0.0.1:6767/health
```

## Hot reload

Every ~250ms the service re-reads the JSON file:

- Valid update → memory updated
- Invalid JSON or malformed `endpoints` → **last known good kept** (warn log)

Startup **fails hard** if the initial file is missing or invalid.

## Configuration

| File | Purpose |
|------|---------|
| [`config/default.toml`](config/default.toml) | Local defaults (`127.0.0.1:6767`) |
| [`config/docker.toml.example`](config/docker.toml.example) | Container template (`0.0.0.0:6767`) |

Paths in TOML that are relative are resolved against the TOML file’s directory.

`config/docker.toml` and `wallet_config.json` are gitignored — create them on the host (or in CI) and bind-mount them. For containers:

```bash
cp config/docker.toml.example config/docker.toml
```

Environment overrides use the `REMOTE_CONFIG` prefix and `__` separator, e.g.:

```bash
REMOTE_CONFIG__SERVER__PORT=9090
REMOTE_CONFIG__REMOTE_CONFIGS__WALLET_CONFIGS_FILE=/path/to/flags.json
REMOTE_CONFIG__EXCHANGE_RATE__API_KEY=your-key
REMOTE_CONFIG__RISK_CHECKER__ETHERSCAN_API_KEY=your-etherscan-key
REMOTE_CONFIG__RISK_CHECKER__INFURA_API_KEY=your-infura-key
```

## Docker

The image ships the binary only. Mount host-managed TOML and JSON at runtime (same pattern IAC should use).

```bash
cp config/docker.toml.example config/docker.toml
cp wallet_config.example.json wallet_config.json

docker build -t remote-config .
docker run --rm -p 6767:6767 \
  -e REMOTE_CONFIG__EXCHANGE_RATE__API_KEY=your-key \
  -e REMOTE_CONFIG__RISK_CHECKER__ETHERSCAN_API_KEY=your-etherscan-key \
  -e REMOTE_CONFIG__RISK_CHECKER__INFURA_API_KEY=your-infura-key \
  -v "$(pwd)/wallet_config.json:/app/wallet_config.json:ro" \
  -v "$(pwd)/config/docker.toml:/app/config/docker.toml:ro" \
  remote-config
```

- Default command: `--config config/docker.toml` (expects that path to be mounted; listens on `0.0.0.0:6767`)
- Mount `wallet_config.json` at `/app/wallet_config.json` (path from the TOML)
- Mount configs **read-only**; the image runs as uid `10001`
- If host files are mode `0640`, run the container as the file owner (IAC pattern: set `user:` to the admin UID) so the process can read them
- Set `REMOTE_CONFIG__EXCHANGE_RATE__API_KEY` and risk-checker keys (or put them in the mounted TOML)

## Development

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
