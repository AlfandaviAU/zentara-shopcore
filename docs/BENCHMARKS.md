# Test results and benchmarks

All numbers below were measured on 2026-11-23 on the same machine (22 CPU cores, Docker VM with 7.25 GiB RAM),
with Postgres 16 and the legacy bank simulator, both via `docker compose`. The load generator (k6) ran on
the same host, so absolute numbers are conservative. Legacy and Rust were run one after the other with the
**identical** script and offered load.

## 1. Functional test results

| Suite | Command | Result |
|---|---|---|
| Unit tests (pricing, rounding parity with Python, parsing, hashing, signatures, CSV, file names) | `cargo test` | **14 / 14 pass** |
| API integration tests (real router and Postgres, mock bank, webhook receiver) | `docker compose --profile test run --rm test` | **19 / 19 pass** |
| CLI tests (the real binary: every maintenance command, plus `serve` with graceful SIGTERM) | same | **3 / 3 pass** |
| Legacy smoke tests, unchanged, run black-box against the Rust service | `docker compose --profile smoke run --rm smoke` | **15 / 15 pass** |
| `cargo clippy --all-targets` | | 0 warnings |
| Clean-checkout build (`git clone` and `docker build --no-cache`) | | pass |

### Code coverage (`cargo llvm-cov`, all suites)

| | Lines | Functions | Regions |
|---|---|---|---|
| Before the coverage work | 69.6% | 69.4% | 66.8% |
| **Now** | **95.3%** | **95.0%** | **91.4%** |

Every route module is at 96–100% of lines, and `main.rs` (the CLI) is at 97%. The remaining uncovered lines are
infrastructure-failure paths (the database staying unreachable for 60 s, panics in the panic handler, unreadable bank responses).
Reproduce with `TEST_DATABASE_URL=… cargo llvm-cov --summary-only`.

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

### 2.3 Scaling beyond the target: 3× the mixed load (`SCALE=3`)

| Metric | Result |
|---|---|
| Requests | 773,565 in ~100 s |
| Throughput | **7,449 req/s** |
| HTTP errors | **0** |
| p95 overall / browse / account | 41.8 ms / 27.2 ms / 50.6 ms |
| p95 create order / wallet pay / bank pay / report | 73.9 ms / 40.7 ms / 334 ms / 162 ms |
| SLO thresholds | **6 of 6 met** |
| Invariant violations / duplicate bank charges | **0 / 0** |
| Database checks afterwards | every `order_no` unique, no negative stock |

The legacy service already failed every SLO at 1×.

### 2.3b Overload: 5× the mixed load (`SCALE=5`)

The service saturates and degrades gracefully. It does not fail.

| Metric | Result |
|---|---|
| Requests / errors | 779,876 / **0** |
| Throughput | 7,483 req/s (the same plateau as 3×: the machine is full, and it also runs k6) |
| p95 overall | 445 ms; latency SLOs are exceeded, and nothing times out |
| Invariant violations / duplicate charges | **0 / 0**; every `order_no` is unique, and no stock is negative |
| App memory | 749 MiB |

### 2.3c Failure tests (run against the stack started from a fresh `git clone`)

| Failure | Behaviour |
|---|---|
| Bank down | bank pay gets a clean **502 in 0.7 s** (3 attempts) and the order stays `pending`, so it can be paid later; wallet pay and browsing keep working; bank pay works again as soon as the bank is back |
| Database restart | the pool reconnects by itself, no failed requests were observed, sessions survive (they live in Postgres), and order + pay work right after |
| App restart | healthy again in **~50 ms**; existing login tokens stay valid |

### 2.4 Breakpoint tests: where each path breaks (`loadtest/breakpoint.js`)

Each traffic class ramps over 60 s until p95 > 1 s or errors > 1%.

| Path | Ceiling on this machine | What limits it |
|---|---|---|
| Anonymous reads (`MODE=read`) | **20,500 req/s**, p95 35 ms, 0 errors (not broken) | the k6 load generator on the same host ran out of CPU first |
| Order + wallet pay over 200 SKUs (`MODE=order SKUS=200`) | **2,678 flows/s** (5,368 req/s), 0 errors | Postgres write throughput (WAL flush) |
| Register (`MODE=login`) | **~265/s** | Argon2id hashing, CPU- and memory-bound by design (~16 ms and 19 MiB per hash) |
| Order + pay on **one** SKU (`MODE=order SKUS=1`, flash sale) | **~100–135 flows/s** | every order for that product updates the same stock row, so commits are serialised |

What these mean:
* **Reads and normal checkout scale far beyond the target load.**
* **Login rate** is bounded by deliberate password-hashing cost. Raising it means more cores or replicas,
  or weaker hashing parameters, which is a security decision and was not taken.
* **A single hot SKU** is the one real ceiling: about 100 orders/s on one product. Reserving stock as the last step before
  commit improved spread-out order throughput by 14% (2,351 to 2,678/s), but a single row is bounded by
  commit latency (WAL flush, ~1.4 ms on this Docker VM). An in-process per-product lock queue was tried and
  **rolled back**: it made the mixed test worse (lock convoys across multi-product orders). Options for flash sales are
  listed in ARCHITECTURE.md §7; each needs a design decision (schema, infrastructure or durability).

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
| v5 | 57 ms at 1×; 41.8 ms p95 at **3×** | 73.9 ms at 3× | – | stock reserved last, one-statement order insert |

## 5. How to reproduce

```sh
docker compose up -d --build
docker compose --profile smoke run --rm smoke                       # parity
docker compose --profile test run --rm test                         # unit + integration
docker run --rm --network host -v "$PWD/loadtest:/lt" grafana/k6:0.54.0 run /lt/shopcore.js
```

To compare, stop this stack, start the legacy stack (`docker compose up -d --build` in the legacy folder), and rerun the k6 command.
`SCALE=2` doubles every arrival rate. Breakpoints: `docker run --rm --network host -e MODE=read -v "$PWD/loadtest:/lt" grafana/k6:0.54.0 run /lt/breakpoint.js`
(the modes are `read`, `auth`, `login` and `order`; `SKUS=N` spreads orders over N products).
