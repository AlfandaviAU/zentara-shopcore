# Test results and benchmarks

All numbers below were measured on 2026-11-23 on the same machine (22 CPU cores, Docker VM with 7.25 GiB RAM),
with Postgres 16 and the legacy bank simulator, both via `docker compose`. The load generator (k6) ran on
the same host, so absolute numbers are conservative. Legacy and Rust were run one after the other with the
**identical** script and offered load.

## 1. Functional test results

| Suite | Command | Result |
|---|---|---|
| Unit tests (pricing, rounding parity with Python, parsing, hashing, signatures, CSV, file names) | `cargo test` | **14 / 14 pass** |
| Integration tests (real router and Postgres, mock bank that drops the first answer) | `docker compose --profile test run --rm test` | **11 / 11 pass** |
| Legacy smoke tests, unchanged, run black-box against the Rust service | `docker compose --profile smoke run --rm smoke` | **15 / 15 pass** |
| `cargo clippy --all-targets` | | 0 warnings |

The integration tests cover:
* 20 concurrent orders for 5 units: exactly 5 succeed, stock ends at 0, and every `order_no` is unique
* 10 concurrent wallet pays: exactly 1 succeeds, charged once
* bank retry after a lost response: exactly 1 charge
* unsigned webhook: rejected
* refunds above the paid amount and repeated refunds: rejected
* another user's order: 404
* self-transfers and negative amounts: rejected
* SQL injection in `sort` / `group_by`: rejected
* path traversal in export and download: rejected
* report freshness: no stale cache
* logout: invalidates the token
* admin routes: need a token

## 2. Load test: realistic mixed traffic (k6)

Script: [`loadtest/shopcore.js`](../loadtest/shopcore.js). It uses an open model (ramping arrival rate),
so a slow server cannot reduce the offered load, which is the industry-standard way to find saturation.

| Scenario | Traffic | Peak rate |
|---|---|---|
| browse | anonymous list / search / sort / product detail | 3,000 iterations/s |
| checkout | register, login, top up, order (20% with coupon), pay by wallet (with a double-click retry) or bank, 10% cancel | 150 iterations/s |
| account | orders, invoice, `/me`, wallet, transfers between 50 users | 300 iterations/s |
| backoffice | sales reports (day / status / user), exports | 5 iterations/s |

The bank is set to 50 ms latency and **drops 10% of responses after recording the charge**, simulating a flaky payment gateway.
Built-in invariant checks:
* order total equals the sum of its lines (with discount)
* wallet balance after payment is exact
* a second pay gets 409
* money is conserved across transfers
* no order is charged twice at the bank

SLO thresholds: error rate < 0.5%; p95 browse < 100 ms, account < 150 ms, create order < 200 ms,
wallet pay < 150 ms, bank pay < 400 ms, report < 500 ms.

### 2.1 Head-to-head

| Metric | Legacy (Flask) | Rust | Improvement |
|---|---|---|---|
| Requests served in ~100 s | 83,170 | **299,187** | **3.6×** |
| Throughput | 710 req/s | **2,940 req/s** | **4.1×** |
| Iterations dropped (server could not keep up) | 174,770 | **122** | ~1,400× fewer |
| Latency p50 (all requests) | 1.47 s | **0.95 ms** | ~1,500× |
| Latency p95 (all requests) | 6.21 s | **57 ms** | **~109×** |
| Browse p95 | 2.92 s | **3.4 ms** | ~860× |
| Account p95 | 8.48 s | **6.8 ms** | ~1,250× |
| Create order p95 | 32.7 s | **15.8 ms** | ~2,000× |
| Wallet pay p95 | 12.4 s | **9.8 ms** | ~1,270× |
| Bank pay p95 (includes 50 ms bank latency and retries) | 9.47 s | **315 ms** | 30× |
| Sales report p95 | 60 s (timeouts) | **66 ms** | ~900× |
| HTTP errors | 51 (0.06%) | **0** | |
| Duplicate bank charges | **87** of 895 | **0** of 5,309 | correctness fix |
| Invariant violations (wrong money) | **88** | **0** | correctness fix |
| SLO thresholds met | 0 of 6 | **6 of 6** | |

### 2.2 Concurrency stress (200 simultaneous order + bank-pay flows, 150 units in stock, bank drops 30%)

| | Legacy | Rust |
|---|---|---|
| Wall time | 8.33 s | **0.80 s** |
| Orders accepted | 200 (oversold by 50) | **exactly 150** |
| Stock counter afterwards | 135 (corrupted by lost updates) | **0 (exact)** |
| Bank charges | 267 for 198 orders (51 orders charged twice) | **150 for 150 orders** |

## 3. Resource footprint

| | Legacy | Rust |
|---|---|---|
| Container image | 219 MB | **138 MB** |
| App restart to healthy | – | **~40 ms** |
| Idle memory | – | **12 MiB** |
| Memory after the full load test | 232 MiB (at 710 req/s) | 440 MiB (at 2,940 req/s, with memory-hard Argon2 hashing) |

Rust uses more memory under load because Argon2id is memory-hard by design (19 MiB per concurrent hash, bounded
by the bulkhead). The legacy service used unsalted MD5, which is cheap and insecure. Even with roughly 1,000× more
work per login, the Rust service handles 4× the throughput.

## 4. Tuning history (Rust, same load)

| Step | p95 overall | Create order p95 | Memory | Change |
|---|---|---|---|---|
| v1 | 353 ms | 962 ms | – | first correct version |
| v2 | 662 ms | 1.07 s | 3.7 GiB | sequence-based `order_no` (the lock bottleneck moved to hashing) |
| v3 | 198 ms | 328 ms | 3.4 GiB | Argon2 bulkhead, access log at debug |
| v4 | **57 ms** | **15.8 ms** | **440 MiB** | mimalloc, bulkhead = cores/2 |

## 5. How to reproduce

```sh
docker compose up -d --build
docker compose --profile smoke run --rm smoke                       # parity
docker compose --profile test run --rm test                         # unit + integration
docker run --rm --network host -v "$PWD/loadtest:/lt" grafana/k6:0.54.0 run /lt/shopcore.js
```

To compare, stop this stack, start the legacy stack (`docker compose up -d --build` in the legacy folder), and rerun the k6 command.
`SCALE=2` doubles every arrival rate.
