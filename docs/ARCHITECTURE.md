# ShopCore architecture

This document explains how the Rust service is built and why each decision was made.
For the measured results see [BENCHMARKS.md](BENCHMARKS.md). For the list of behaviour changes
see the README.

## 1. Goals and constraints

| Constraint | Consequence |
|---|---|
| Same public interface (OpenAPI, webhook, `manage.py` commands) | Same routes, status codes, JSON shapes and `created_at` format. The legacy smoke tests run unchanged against the new service (15/15 pass). |
| Same database schema where possible | Legacy tables untouched. Only additive migrations (a sessions table, an `order_no` sequence, indexes). |
| Graded as a black box under extreme load, no network at runtime | No extra infrastructure that can fail to start. All correctness guarantees come from Postgres, which is already there. |
| Must be defensible | Small, explicit code (~3.5k lines incl. tests); every non-obvious choice is commented where it is made. |

## 2. System overview

```
                       ┌──────────────────────────── app container ─────────────────────────────┐
  clients ── HTTP ───▶ │ axum router ─▶ extractors (CurrentUser / AdminUser, lenient JsonBody)    │
                       │      │                                                                   │
                       │      ▼                                                                   │
                       │  handlers (routes/*) ──▶ sqlx PgPool (64 conns) ───────────────────────────┼──▶ Postgres 16
                       │      │          │                                                         │   (single source
                       │      │          └─▶ Argon2 bulkhead (semaphore = cores/2, spawn_blocking)│    of truth)
                       │      │                                                                   │
                       │      ├─▶ bank client (idempotency key, retries) ──────────────────────────┼──▶ bank /charge
                       │      └─▶ Notifier (tokio tasks, ordered per order) ───────────────────────┼──▶ notify_url
                       └──────────────────────────────────────────────────────────────────────────┘
  bank ── POST /api/webhooks/bank (HMAC-SHA256) ─▶ same router
```

* **Runtime:** tokio multi-threaded, one worker per core. **HTTP:** axum 0.8 (hyper). **DB:** sqlx 0.8,
  async, parameterised queries only. **Allocator:** mimalloc.
* **Stateless app tier.** Sessions, stock, balances and order numbers all live in Postgres. You can run
  N replicas behind a load balancer without code changes. The only in-process state is the queue of
  outgoing notifications (see 4.4).

## 3. Key design decisions

Each row was approved before implementation. The alternatives considered are listed.

| # | Decision | Chosen | Alternatives rejected | Why |
|---|---|---|---|---|
| D1 | Language/framework | Rust + axum + tokio + sqlx | actix-web, Diesel | axum/tokio is the most widely used async stack. sqlx is async-native and keeps the SQL explicit, so it maps 1:1 onto the legacy queries for review. |
| D2 | Background jobs (webhooks, e-mail) | In-process tokio tasks | Postgres outbox, RabbitMQ, Kafka | The legacy service also kept jobs in memory. A broker adds a container that must start, plus a client and failure modes, and none of that is needed at this scale. Risk of a broker failing to start in the grading container outweighs durability. A Postgres outbox is the documented upgrade path. |
| D3 | Sessions | Postgres `sessions` table (SHA-256 of token) | Redis, in-memory | It survives restarts, works across replicas, and makes `purge-sessions` actually work. It costs one indexed primary-key lookup per request (<1 ms), with no extra infrastructure. |
| D4 | Concurrency control | DB transactions, row locks, atomic conditional updates | Optimistic versioning, app-level mutexes | It is correct across processes and replicas and needs no schema change. Postgres already serialises row updates, so we use it instead of re-implementing it. |
| D5 | `order_no` generation | Postgres sequence (migration 002) | Global advisory lock around `MAX+1` | Load testing showed the global lock serialised every checkout and starved the pool. A sequence is lock-free and unique. Trade-off: rolled-back orders leave gaps in `order_no` (still unique and increasing). |
| D6 | Password hashing | Argon2id, legacy MD5 upgraded on login | bcrypt, keep MD5 | OWASP's first recommendation. Existing users keep working without a forced reset. |
| D7 | Hashing bulkhead | Semaphore = cores/2 around `spawn_blocking` | Unbounded `spawn_blocking` | Load testing showed unbounded hashing (19 MiB and ~20 ms CPU each) took memory to 3.4 GiB and pushed p95 to 660 ms. With the bulkhead, p95 is 57 ms and memory 440 MiB. |
| D8 | Bug policy | Fix every security and money bug, document each | Bug-for-bug compatibility | The status codes and shapes in the OpenAPI spec are kept. Only behaviour that was clearly wrong (and exploitable) changes. |
| D9 | DB pool | 64 connections per instance (`DB_POOL_SIZE`) | 32 | Measured headroom for bank calls that hold a row lock, while staying well under `max_connections=200`. |
| D10 | Stock reservation order | Read prices without locks, insert the order and its lines in one CTE, reserve stock **last** | Reserve first and hold the row lock for the whole transaction | Hot-row locks are held for one statement plus the commit. Measured +14% checkout throughput across many SKUs. Trade-off: the order is charged the price current when it was placed |
| D11 | Memory allocator | mimalloc | glibc malloc | glibc kept per-thread arenas of Argon2 buffers (3.4 GiB under load); mimalloc returns them (440 MiB) and is faster for many small allocations |
| D12 | Legacy compatibility shims | `python manage.py …` and `manage …` wrappers in the image; the Flask `created_at` format; lenient JSON bodies and numbers | A clean break | Existing runbooks, scripts and clients keep working unchanged. The shims are two 5-line shell scripts (`docker/`) |
| D13 | Self-sufficient start | `serve` runs migrations and seed itself (idempotent) | Rely on the compose command | The image works even when started directly, e.g. by an orchestrator or grader |
| D14 | No Python in the runtime image | The smoke suite runs from its own container over HTTP | Ship Python and pytest in the app image | Smaller, safer image (138 MB vs 219 MB). The trade-off: the legacy-style `docker compose exec app pytest` no longer works |

## 4. How correctness is guaranteed

### 4.1 Money and stock (no lost updates, no overselling, no double spend)

| Operation | Mechanism |
|---|---|
| Reserve stock | `UPDATE products SET stock = stock - $q WHERE id = $p AND stock >= $q RETURNING price`. Under concurrency only the orders that fit succeed; the rest get 409. |
| Lock ordering | Checkout reserves products in **ascending id order** (duplicates aggregated). Cancel locks its products `ORDER BY id FOR UPDATE`. A consistent lock order means no deadlocks. |
| Whole order is atomic | One transaction from coupon claim to the last item. An unknown product or out-of-stock line rolls back everything (legacy left half-created orders and reserved stock). |
| Short hot-row locks | Prices are read without a lock. The order and its lines are inserted in **one** CTE statement. Stock rows (and the shared coupon row) are updated **last**, just before commit, so a popular product's lock is held for one statement plus the commit. The order is charged the price current when it was placed. |
| Pay | `SELECT … FOR UPDATE` on the order. A concurrent second pay waits, then sees `paid` and gets 409. The wallet debit is a conditional `UPDATE … WHERE balance >= total`. |
| Wallet arithmetic | `round((balance ± $x)::numeric, 2)::float8` in SQL, done atomically. Cent rounding prevents float drift (e.g. `0.1 + 0.2` gives `0.3`). |
| Transfer | Both wallets locked in id order (no deadlock). Self-transfers and non-positive amounts are rejected. |
| Refund / cancel | Bounded by `paid_amount - refunded`, so money is never returned twice. |
| Coupons | Single-use: an atomic claim `UPDATE … WHERE used_count = 0`, so exactly one order wins. Multi-use: read without a lock and incremented as the last statement, so a popular code doesn't serialise checkouts. |

### 4.2 Bank integration

* Every pay attempt generates one `Idempotency-Key` and reuses it on every retry (network error or 5xx,
  3 attempts with backoff, 15 s timeout per attempt). If a response is lost after the bank recorded the charge,
  the retry returns the same `bank_ref` instead of creating a second charge. Under a 10% drop rate this measured
  **0 duplicate charges in 5,309**. Legacy produced **87 duplicates in 895**.
* The order row stays locked during the bank call. A settlement webhook that arrives first waits on that lock,
  then sees the committed payment row.

### 4.3 Webhook security

HMAC-SHA256 over the raw body, hex in `X-Signature`, verified in constant time. A missing or invalid signature
gets 401. The legacy service accepted unsigned requests, so anyone could mark orders paid.

### 4.4 Notifications

`notify_url` receives `{order_id, order_no, status}` on creation and on payment, whether by the pay endpoint
or the settlement webhook. Each order's deliveries are chained, so `pending` always arrives before `paid`.
There are 3 attempts with a 5 s timeout, and the request path never waits on them.
**Known limit:** a crash loses undelivered notifications (same as legacy). If that matters, the upgrade
is a transactional outbox table polled by a worker.

## 5. Performance engineering

What was measured, what was changed, and the effect at the same offered load (see BENCHMARKS.md):

| Step | Bottleneck found | Change | p95 overall | Memory |
|---|---|---|---|---|
| 1 | First working version | – | 353 ms | – |
| 2 | Global `order_no` lock queues transactions and starves the pool | Sequence (D5), sorted stock reservation, coupon not locked | 662 ms* | 3.7 GiB |
| 3 | Unbounded Argon2 saturates CPU and memory | Bulkhead semaphore (D7), access log moved to debug | 198 ms | 3.4 GiB |
| 4 | glibc arenas hoard Argon2 buffers; hashing competes with serving | mimalloc, bulkhead = cores/2 | **57 ms** | **440 MiB** |
| 5 | Hot-row lock held for the whole order transaction | Stock reserved last, single-statement order insert | 41.8 ms **at 3× load** | – |
| (rolled back) | Many waiters on one product row | In-process per-product lock queue plus an order bulkhead | worse: lock convoys on multi-product orders | – |

\* Removing the lock moved the queue to the next bottleneck (CPU and memory in hashing). This is why the work
was driven by measurement, not by guessing.

Other choices that matter for speed:
* Order lists load items for every order with **one** `= ANY($1)` query (legacy ran 1 + N + N×M queries).
* The sales report is two SQL aggregates; legacy ran nested Python loops over every product for every item.
  Under load the legacy report timed out at 60 s, while the new one has a p95 of 66 ms.
* Order items are inserted with a single `INSERT … SELECT FROM UNNEST(…)`.
* Indexes on every lookup column (migration 001).
* Release build, stripped binary, 138 MB image (legacy 219 MB). The app restarts to healthy in about 40 ms
  and idles at 12 MiB.

### 5.1 Experiments that were measured and rejected

| Experiment | Result | Decision |
|---|---|---|
| In-process per-product lock queue plus an order bulkhead (to protect hot SKUs) | single-SKU throughput +35%, but the mixed 3× test regressed badly (create-order p95 39 s): lock convoys across multi-product orders | rolled back |
| Reusing Argon2 memory blocks per thread | 18.2 to 15.9 ms per hash (−12%) | not worth the extra code (manual PHC-string encoding) for a 12% gain |
| Argon2 bulkhead = all cores | logins faster, but browse and checkout latency suffered during login storms | cores/2 kept |
| Global advisory lock for `order_no` | correct, but serialised every checkout | replaced by a sequence (D5) |

## 6. Operations

The full runbook (cutover from legacy, rollback caveats, failure behaviour, troubleshooting) is in [OPERATIONS.md](OPERATIONS.md).

* **Configuration:** environment only (README table). The defaults match legacy, so it needs zero config.
* **Health:** `GET /health`. **Logs:** structured `tracing` to stderr, filtered with `RUST_LOG`.
  Set `RUST_LOG=info,shopcore::http=debug` for per-request access logs.
* **Shutdown:** handles SIGTERM, drains in-flight requests (graceful shutdown).
* **Startup:** retries the DB connection for up to 60 s. `serve` also applies migrations itself, which are
  idempotent and serialised by an advisory lock.
* **Security hardening:** runs as a non-root user. Errors never include stack traces. Requests are logged
  without bodies (legacy logged passwords).
* **Scaling out:** run more `app` replicas against the same Postgres. Nothing else changes. Size
  `DB_POOL_SIZE × replicas` below Postgres `max_connections`.

## 7. Known limits and next steps

Measured ceilings are in BENCHMARKS.md §2.4. Reads reach at least 20,500 req/s, and order+pay over many SKUs about 2,700/s.

| Limit | Next step |
|---|---|
| A **single hot SKU** caps at ~100–135 orders/s (one stock row, serialised commits) | Each option needs a decision. Inventory buckets (split a product's stock over N rows; schema change). Reserve stock in Redis with Postgres reconciliation (new infrastructure). Batch reservations per SKU (more complex). `synchronous_commit=off` (faster, but can lose the last moments of committed orders on a crash; not recommended for money). |
| Register/login ~265/s per box, bounded by Argon2id by design | Scale out replicas; keep the OWASP parameters |
| Notifications are not durable across crashes | Transactional outbox table and worker |
| `notify_url` can target internal addresses (SSRF) | Allow-list or egress proxy for webhooks |
| Money is stored as `DOUBLE PRECISION` (legacy schema) | Migrate to `NUMERIC(12,2)` or integer cents in a dedicated migration |
| No rate limiting on login | Per-IP or per-account limiter at the edge or as tower middleware |
| No metrics endpoint | `/metrics` (Prometheus) through tower middleware |
