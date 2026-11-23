# Operations runbook

## Run

```sh
docker compose up -d --build          # db + bank simulator + app on :8000
curl localhost:8000/health            # {"status":"ok"}
docker compose logs -f app
```

Ports already taken? Run `APP_PORT=18000 BANK_PORT=15001 docker compose up -d --build`.

The app container runs `shopcore init-db && shopcore seed && shopcore serve`. `serve` also applies the migrations and seeds by itself.
All three steps are idempotent, so restarts and running the image directly are both safe.

## Configuration

All configuration is through environment variables; the table is in the README. Points that matter in production:

| Setting | Guidance |
|---|---|
| `ADMIN_PASSWORD` | change it from the default `admin123` before first start (seed only creates the admin if missing) |
| `BANK_SECRET` | must match the bank's signing secret; requests with a wrong or missing signature get 401 |
| `DB_POOL_SIZE` | per replica. Keep `DB_POOL_SIZE × replicas` below Postgres `max_connections` (200 in compose) |
| `SESSION_TTL_HOURS` | token lifetime (24 h); `shopcore purge-sessions` logs everyone out immediately |
| `BANK_TIMEOUT_MS`, `BANK_ATTEMPTS` | worst-case pay latency is about attempts × timeout; defaults 3 × 15 s |
| `RUST_LOG` | `info,sqlx=warn` by default; add `,shopcore::http=debug` for a per-request access log |

## Cutover from the legacy service (same database)

1. **Stop the legacy app.** The two must not write to the same database at the same time, because legacy computes `order_no` as `MAX+1`.
2. Start the Rust app against the existing database. `init-db` runs automatically:
   * the legacy tables are left as they are;
   * migration 001 adds the `sessions` table and indexes;
   * migration 002 creates `orders_order_no_seq`, aligned to the current `MAX(order_no)`, so numbering continues.
3. Users must **log in again** once (legacy sessions lived in process memory and are already gone after any restart).
4. Passwords need no reset. Legacy MD5 hashes are verified and upgraded to Argon2id on each user's next login.

### Rollback to legacy

The schema change is additive, so the legacy app still runs against the database. Two caveats:
* users whose hash was **upgraded to Argon2id** can't log in to legacy (it only checks MD5). They need a password reset,
  or you restore their old MD5 hashes from a backup taken before cutover. **Take a backup before cutover.**
* the extra `sessions` table, sequence and indexes are harmless to legacy and can stay.

## Scaling

* The app tier is **stateless**: sessions, stock, balances and order numbers are all in Postgres. Add replicas behind a
  load balancer with no code changes.
* The only per-process state is queued `notify_url` deliveries. They are lost if a process crashes, the same as legacy.
* Measured on one 22-core host (BENCHMARKS.md): 7,449 req/s mixed traffic with all latency SLOs met; 20,500 req/s
  catalogue reads; about 2,700 checkouts/s across many products; about 265 registrations/s (Argon2 is costly by design).
* The known ceiling is a **flash sale on a single product** (about 100–135 orders/s on one stock row). See ARCHITECTURE.md §7.

## Failure behaviour (measured, BENCHMARKS.md §2.3c)

| Failure | What happens | Action |
|---|---|---|
| Bank unreachable or erroring | bank pay gets a 502 after 3 attempts; the order stays `pending` and can be paid again; everything else works | none; payments resume when the bank is back. A retry never double charges (idempotency key) |
| Settlement webhook lost | the order is already `paid` (pay returned it); only the payment row stays `captured` | none |
| Postgres restart | the pool reconnects by itself; sessions survive | none |
| Postgres down at app start | the app retries the connection for 60 s, then exits non-zero | fix the DB, then `docker compose up -d app` (compose sets no restart policy; add `restart: unless-stopped` in production) |
| App crash or restart | healthy again in about 50 ms; tokens stay valid; queued notifications are lost | none |
| Overload | latency rises and requests don't fail (5× load: 0 errors, p95 445 ms) | add replicas |
| Login storm | Argon2 runs on at most half the cores, so other endpoints stay fast while logins queue | add replicas |

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `Bind for 0.0.0.0:8000 failed: port is already allocated` | another stack (for example legacy) is running; stop it or set `APP_PORT` / `BANK_PORT` |
| All webhooks get 401 | `BANK_SECRET` differs between the app and the bank |
| `could not connect to the database` after 60 s | wrong `DATABASE_URL` or the DB isn't healthy |
| `python manage.py …` prints "python is not installed" | only `manage.py` and `run.py` are mapped by the shim; use `shopcore <command>` |
| Export download gives 404 | file names must be plain (`[A-Za-z0-9._-]`); files live in `EXPORT_DIR` (`/tmp/exports`) inside the container |
