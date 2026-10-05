# Quersi (QRC) - Quantus Remote Config

Minimal Axum service that serves wallet feature flags, a NEAR Intents geo gate, USD exchange rates, and Ethereum address risk reports over HTTP.

## Design

Feature flags are intentionally **public to read**. There is **no write/admin HTTP API** — updates are done by replacing the JSON file on disk (hot-reloaded).

Signing config files does not help against a compromised host: anyone who can replace the service binary (or its config) can serve arbitrary responses anyway. Trust is host/deploy integrity, not a signature on the JSON.

```text
Operator/CI --> wallet_config.json --> VPS mount
                geo_config.json     |
                          remote-config (load / hot-reload)
                                    |
                          GET /api/configs/wallet   (+ geoNearAllowed for the caller)
                          GET /api/geo/near-allowed

ExchangeRate-API v6  -->  in-memory cache  -->  GET /api/exchange-rates

Infura (ENS) + Etherscan  -->  GET /api/risk-checker/{address_or_ens}
```

## API

| Method | Path | Auth | Response |
|--------|------|------|----------|
| `GET` | `/api/configs/wallet` | none | `{ "data": { ...flags, "geoNearAllowed": bool } }` |
| `GET` | `/api/geo/near-allowed` | none | `{ "data": { "ip", "country", "nearAllowed" } }` |
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
    "geoNearAllowed": false
  }
}
```

Payload is opaque JSON: add or change keys by updating the file — no service redeploy required. Only invalid JSON is rejected. `geoNearAllowed` is not in the file: the geo gate adds it per request.

## NEAR Intents geo gate

NEAR Intents is not available everywhere. The wallet hides its swap button where it is not, and this service decides where that is. The decision rides along in `/api/configs/wallet` as `geoNearAllowed`, so the wallet learns it in the request it already makes; `/api/geo/near-allowed` shows the same verdict with what it was based on, for checking a block list from the place it is meant to block.

The service runs behind Cloudflare, which stamps every proxied request with the visitor's country (`CF-IPCountry`, ISO 3166-1 alpha-2) and IP (`CF-Connecting-IP`). The gate reads those headers and nothing else; there is no IP database to keep current. Blocking a country is one line in `geo_config.json`, hot-reloaded like the flags:

```json
{
  "nearBlockedCountries": ["RU", "IR", "KP"],
  "nearBlockedRanges": ["203.0.113.0/24"],
  "nearAllowedWhenUnknown": false
}
```

- `nearBlockedCountries`: country codes as Cloudflare reports them. Case and whitespace are normalized; anything that is not two letters is refused. The list of countries where NEAR Intents is unavailable is NEAR Intents' terms, not this file's; keep them in step.
- `nearBlockedRanges`: CIDR ranges (v4 or v6) blocked whatever their country. They are checked against every client address the headers report: `CF-Connecting-IPv6` (the real address when Cloudflare's Pseudo IPv4 rewrites the others), `CF-Connecting-IP`, the first `X-Forwarded-For` address and `X-Real-IP`.
- `nearAllowedWhenUnknown`: what to answer when no country header is present or Cloudflare reports `XX` or `T1` (Tor). Off by default: a missing header closes the gate rather than opening it.

An unknown key, a malformed country or an unparsable range is rejected: at startup the service fails, on reload the last good config stays and a warning is logged.

`geo_config.example.json` starts from the NEAR terms of service (Polar Express Limited, 2026-02-24), whose prohibited jurisdictions are Afghanistan, Belarus, the Central African Republic, Cuba, the Democratic Republic of Congo, Guinea-Bissau, Haiti, Iran, Libya, Mali, Myanmar, Nicaragua, North Korea, Russia, Somalia, South Sudan, Sudan, Syria, Venezuela, Yemen and Zimbabwe, plus the Crimea, Donetsk, Luhansk, Zaporizhzhia and Kherson regions of Ukraine. Cloudflare reports those regions as `UA` like the rest of the country, so the example blocks Ukraine as a whole. The list is a compliance call: review it before the first deploy and whenever the terms change.

Exchange rates are fetched from [ExchangeRate-API](https://www.exchangerate-api.com/) (v6, USD base) and cached in memory until `time_next_update_unix`.

Risk reports resolve an Ethereum address or `.eth` ENS name (via Infura) and fetch balance / transaction / contract data from Etherscan. Invalid input returns `400`; unresolved ENS or missing address data returns `404`; upstream rate limits return `429`.

CORS is off by default (`cors_allowed_origins = []`). Set origins in TOML when browser clients need cross-origin access.

## Quick start

`wallet_config.json` is gitignored (host-managed). Copy the example first:

```bash
cp wallet_config.example.json wallet_config.json
cp geo_config.example.json geo_config.json
cargo run -- --config config/default.toml
# GET http://127.0.0.1:6767/api/configs/wallet
# GET -H 'CF-IPCountry: GB' http://127.0.0.1:6767/api/geo/near-allowed
# GET http://127.0.0.1:6767/api/exchange-rates
# GET http://127.0.0.1:6767/api/risk-checker/0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045
# GET http://127.0.0.1:6767/health
```

## Hot reload

Every ~250ms the service re-reads each JSON file (`wallet_config.json`, `geo_config.json`):

- Valid update → memory updated
- Invalid JSON → **last known good kept** (warn log)

Startup **fails hard** if the initial file is missing or invalid.

## Configuration

| File | Purpose |
|------|---------|
| [`config/default.toml`](config/default.toml) | Local defaults (`127.0.0.1:6767`) |
| [`config/docker.toml.example`](config/docker.toml.example) | Container template (`0.0.0.0:6767`) |

Paths in TOML that are relative are resolved against the TOML file’s directory.

`config/docker.toml`, `wallet_config.json` and `geo_config.json` are gitignored — create them on the host (or in CI) and bind-mount them. For containers:

```bash
cp config/docker.toml.example config/docker.toml
```

Environment overrides use the `REMOTE_CONFIG` prefix and `__` separator, e.g.:

```bash
REMOTE_CONFIG__SERVER__PORT=9090
REMOTE_CONFIG__REMOTE_CONFIGS__WALLET_CONFIGS_FILE=/path/to/flags.json
REMOTE_CONFIG__GEO__CONFIG_FILE=/path/to/geo.json
REMOTE_CONFIG__EXCHANGE_RATE__API_KEY=your-key
REMOTE_CONFIG__RISK_CHECKER__ETHERSCAN_API_KEY=your-etherscan-key
REMOTE_CONFIG__RISK_CHECKER__INFURA_API_KEY=your-infura-key
```

## Docker

The image ships the binary only. Mount host-managed TOML and JSON at runtime (same pattern IAC should use).

```bash
cp config/docker.toml.example config/docker.toml
cp wallet_config.example.json wallet_config.json
cp geo_config.example.json geo_config.json

docker build -t remote-config .
docker run --rm -p 6767:6767 \
  -e REMOTE_CONFIG__EXCHANGE_RATE__API_KEY=your-key \
  -e REMOTE_CONFIG__RISK_CHECKER__ETHERSCAN_API_KEY=your-etherscan-key \
  -e REMOTE_CONFIG__RISK_CHECKER__INFURA_API_KEY=your-infura-key \
  -v "$(pwd)/wallet_config.json:/app/wallet_config.json:ro" \
  -v "$(pwd)/geo_config.json:/app/geo_config.json:ro" \
  -v "$(pwd)/config/docker.toml:/app/config/docker.toml:ro" \
  remote-config
```

- Default command: `--config config/docker.toml` (expects that path to be mounted; listens on `0.0.0.0:6767`)
- Mount `wallet_config.json` at `/app/wallet_config.json` and `geo_config.json` at `/app/geo_config.json` (paths from the TOML)
- Mount configs **read-only**; the image runs as uid `10001`
- If host files are mode `0640`, run the container as the file owner (IAC pattern: set `user:` to the admin UID) so the process can read them
- Set `REMOTE_CONFIG__EXCHANGE_RATE__API_KEY` and risk-checker keys (or put them in the mounted TOML)

## Development

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
