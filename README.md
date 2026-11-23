# ShopCore (Rust)

ShopCore's order, wallet and reporting backend, migrated from Python/Flask to Rust
(axum + tokio + sqlx on Postgres 16). It exposes the same public interface:

- the HTTP API in [`docs/openapi.yaml`](docs/openapi.yaml)
- the bank settlement webhook (`POST /api/webhooks/bank`, HMAC-SHA256 signed)
- the `manage.py` maintenance commands: `init-db`, `seed`, `import-products <csv>`, `export-sales <file>`, `purge-sessions`

## Build and run

```sh
docker compose up --build
```

This starts Postgres, the bank simulator (`bank_sim/`, unchanged from legacy) and the app on `:8000`.
On first start the app creates the tables and seeds the admin account (`admin` / `admin123`),
two coupons and the sample products. The build needs no manual steps, and the image runs with no network access.

If ports 8000 or 5001 are already in use: `APP_PORT=18000 BANK_PORT=15001 docker compose up --build`.

## Tests

| What | Command |
|---|---|
| Unit tests (no DB needed) | `cargo test` |
| Unit + integration tests (real Postgres, in Docker) | `docker compose --profile test run --rm test` |
| Integration tests from the host | `TEST_DATABASE_URL=postgresql://shop:shop@localhost:5432/shop cargo test` |
| Legacy smoke tests against this service (black-box parity) | `docker compose --profile smoke run --rm smoke` |
| Load test with SLO thresholds (k6) | `docker run --rm --network host -v "$PWD/loadtest:/lt" grafana/k6:0.54.0 run /lt/shopcore.js` |

The integration tests (`tests/api.rs`) start the real router and a mock bank in-process.
The mock bank drops the first answer to every charge. They cover concurrent orders
(no overselling, unique `order_no`), concurrent payments (charged once), bank retry
idempotency, webhook signatures, refund limits, order privacy, wallet validation,
SQL injection, path traversal and report freshness.
Without a database URL the integration tests skip themselves.

## Maintenance commands

Inside the container the binary is `shopcore`. The legacy entry points still work:

```sh
docker compose exec app shopcore export-sales /tmp/exports/sales.csv
docker compose exec app python manage.py purge-sessions   # compat shim, no Python in the image
docker compose exec app manage import-products /path/products.csv
```

`shopcore serve` runs the HTTP server.

## Configuration (environment)

| Variable | Default | Meaning |
|---|---|---|
| `DATABASE_URL` | `postgresql://shop:shop@localhost:5432/shop` | Postgres connection |
| `BANK_URL` | `http://localhost:5001` | Bank simulator base URL |
| `BANK_SECRET` | `bank-shared-secret-2019` | Webhook HMAC secret |
| `BANK_TIMEOUT_MS` / `BANK_ATTEMPTS` | `15000` / `3` | Per-attempt timeout and retry count for `/charge` |
| `ADMIN_USER` / `ADMIN_PASSWORD` | `admin` / `admin123` | Account created by `seed` |
| `EXPORT_DIR` | `/tmp/exports` | Where report exports are written |
| `BIND_ADDR` | `0.0.0.0:8000` | Listen address |
| `SESSION_TTL_HOURS` | `24` | Bearer token lifetime |
| `DB_POOL_SIZE` | `64` | Max Postgres connections |
| `RUST_LOG` | `info,sqlx=warn` | Log filter |

## Design

- **Stateless HTTP tier** on tokio. All shared state is in Postgres, so you can run several replicas behind a load balancer.
- **Consistency comes from the database**, not from application memory:
  - stock is reserved with `UPDATE … SET stock = stock - $q WHERE stock >= $q`
  - wallet debits are conditional updates, and money is rounded to cents in `numeric`
  - pay, cancel, refund and webhook take a `SELECT … FOR UPDATE` lock on the order
  - transfers lock both wallets in id order, so they cannot deadlock
  - order creation is one transaction; `order_no` comes from a sequence, and stock is reserved in product-id order, so there are no deadlocks
- **Bank charges are idempotent.** Every pay attempt sends one `Idempotency-Key`, reused across retries.
- **Background work runs as in-process tokio tasks:** `notify_url` webhooks and e-mails (written to the log).
  Deliveries for one order are chained, so `pending` always arrives before `paid`.
  A message broker (Kafka, RabbitMQ) was considered and rejected for this scope. The trade-off is that
  undelivered notifications are lost on a crash, the same as legacy.
- **Errors** are always `{"error": "..."}`, including unknown routes (404) and wrong methods (405).
  Stack traces are never sent to clients.

### Performance (same k6 mixed-traffic test, same machine)

| | Legacy | Rust |
|---|---|---|
| Throughput | 710 req/s | **2,940 req/s** (4.1x) |
| p95 latency | 6.21 s | **57 ms** (~109x) |
| Duplicate bank charges | 87 | **0** |
| Money invariant violations | 88 | **0** |
| SLOs met | 0 of 6 | **6 of 6** |

Full results: [docs/BENCHMARKS.md](docs/BENCHMARKS.md). Design decisions and their reasons: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Database changes

The legacy schema is unchanged. `init-db` applies one **additive** migration (`MIGRATION_001` in `src/db.rs`):

- a new `sessions` table (`token_hash`, `user_id`, `created_at`, `expires_at`)
- lookup indexes on existing columns (`users.username`, `users.email`, `products.sku`, `coupons.code`,
  `orders.user_id`, `orders.created_at`, `order_items.order_id`, `payments.order_id`)

Migration 002 (`MIGRATION_002`) adds a sequence `orders_order_no_seq` for `order_no`, aligned to the
current `MAX(order_no)` on every `init-db`, so numbering continues and the first order is 1001. Order
creation no longer has to be serialised. Rolled-back orders leave gaps, but numbers stay unique and increasing.

Nothing is altered or dropped. Do not run the legacy service against the same database at the same time,
because it still computes `MAX+1`.

## Behaviour deliberately changed (legacy bugs fixed)

**Security**
1. Admin routes returned data to requests with **no token**; they now return 401 (and 403 for non-admins).
2. `PATCH /api/users/me` wrote any JSON key into SQL (`{"role":"admin"}`, `wallet_balance`, SQL injection).
   Only `name` and `email` are applied now; other keys are ignored, and a duplicate email gets 409.
3. SQL injection via `sort` (products) and `group_by` (reports): both are now whitelisted, and other values get 400.
   Invalid `from`/`to` dates get 400.
4. The webhook accepted requests **without** `X-Signature`. The signature is now required and compared in constant time.
5. Export ran `gzip` through a shell with the user's filename, and download allowed `../` traversal.
   Filenames are now restricted to `[A-Za-z0-9._-]`, and gzip runs in-process.
6. Passwords were unsalted MD5, and tokens were `md5(username:time)` (guessable, and equal for two logins in the same second).
   Passwords now use Argon2id, and legacy MD5 hashes are upgraded on the next login. Tokens are 256-bit random,
   and only their SHA-256 is stored.
7. `GET /api/admin/users` leaked `password_hash`; removed. 500 responses leaked stack traces; removed.
8. Any user could read, cancel or refund **other users' orders**. Orders are now visible to the owner and admins only (404 otherwise).
9. `import-products` built its SQL by string formatting. It is now parameterised.

**Money and stock correctness**

10. Read-modify-write on balances and stock lost updates under concurrency. Every such update is now atomic.
11. Concurrent pays could double-debit, and bank retries had no idempotency key (double charges). Both fixed.
12. Refunds had no limit and could repeat forever, and cancelling a refunded order credited the total again.
    Refunds are now bounded by `paid_amount - refunded`, and cancel credits only what is still unrefunded.
13. Self-transfer created money, and negative or zero amounts were accepted (a negative transfer pulled money
    from the recipient). Self-transfers and non-positive amounts now get 400.
14. Negative or zero quantities reduced the price and increased stock; they now get 400.
    A failed order (unknown product, no stock) no longer leaves a half-created order with stock reserved.
15. Admin-set `shipped` on an unpaid order, followed by cancel, credited money that was never paid. Fixed by crediting from `paid_amount`.
16. A duplicate `order_no` was possible under concurrency (`MAX+1` with no lock). It now comes from a sequence (migration 002).
17. `POST /api/admin/orders/{id}/status` and `/role` accepted any value. They are now limited to the OpenAPI enums (400 otherwise).

**Other**

18. The sales report cache was never invalidated, so reports were stale. It has been removed;
    the queries are now single aggregates instead of N×M Python loops.
19. Logout did not end the session; it does now. `purge-sessions` had no effect, because it cleared another process's memory;
    it now clears the sessions table. Sessions survive restarts (until `SESSION_TTL_HOURS`).
20. Timestamps are stored in **UTC**. Legacy mixed container-local time (`TZ=Asia/Jakarta`) for orders with database time elsewhere.
    `created_at` is still shown in the same RFC 1123 format Flask produced (`Mon, 23 Nov 2026 10:00:00 GMT`).
21. `import-products` now updates existing SKUs instead of inserting duplicates, and fails the whole file on an invalid row
    (legacy aborted halfway). It parses quoted CSV fields correctly, and export CSVs are properly quoted.
22. A paid webhook now also sends the `paid` notification to `notify_url`, and sets `paid_amount`.
    Topup and transfer amounts must be positive numbers (400 otherwise).
23. The in-memory audit log, which kept every request body including passwords, was replaced by an access log without bodies.

## Layout

```
src/main.rs        CLI: serve + maintenance commands
src/routes/        HTTP handlers (users, products, orders, payments, wallet, reports, admin, webhooks)
src/auth.rs        password hashing, sessions, CurrentUser / AdminUser extractors
src/db.rs          schema + additive migrations, connection with retry
loadtest/          k6 load test (SLO thresholds + invariant checks)
docs/              ARCHITECTURE.md, BENCHMARKS.md, openapi.yaml
src/bank.rs        idempotent bank client
src/notify.rs      background webhooks / e-mails
src/manage.rs      seed, import, export, purge
tests/api.rs       integration tests (real Postgres + mock bank)
smoke/             legacy smoke tests (black-box parity)
bank_sim/          bank simulator from the legacy repo
```
