# API compatibility: legacy vs Rust

Every public interface, endpoint by endpoint: what stayed the same and what changed, and why.
"Same" means the same path, method, request fields, response shape and status codes. The legacy smoke tests
(`smoke/test_smoke.py`, unchanged) pass against the Rust service. Every change below fixes a bug, and each one
is covered by a test in `tests/api.rs` or `tests/cli.rs` (see [TESTING.md](TESTING.md)).

## Conventions (all endpoints)

| Aspect | Legacy | Rust |
|---|---|---|
| Error body | `{"error": …}` for handled errors; **500 included `trace`** (a stack trace) | always `{"error": …}`. A 500 is `{"error": "internal server error"}`, and details go to the server log only |
| Unknown route / wrong method | Flask HTML page | JSON `404 {"error":"not found"}` / `405 {"error":"method not allowed"}` |
| Path ids (`/orders/{id}`) | Flask `<int:>`: non-digits gave 404 | same: digits only, otherwise 404; values beyond int32 also 404 |
| Request body | `get_json(silent=True) or {}` | same leniency: a missing, malformed or non-object body counts as `{}` |
| Numeric fields | lenient `to_int` / `to_money` | same parsing (`"12"`, `"5abc"` → 5, `2.9` → 2); invalid **money** values now give 400 instead of silently becoming 0 |
| `created_at` | Flask RFC 1123 string, e.g. `Mon, 23 Nov 2026 10:00:00 GMT` | **same format**; values are now stored in UTC (legacy mixed container-local Asia/Jakarta time with database time) |
| Money values | floats, unrounded drift (`0.30000000000000004`) | floats rounded to cents in SQL; totals use Python's `round(x, 2)` semantics exactly |
| Auth header | `Authorization: <anything> <token>` | same |
| Tokens | `md5(username:unix_seconds)`: guessable, and two logins in the same second got the same token | 256-bit random hex; only its SHA-256 is stored |
| Sessions | in process memory: lost on restart, never ended by logout | Postgres `sessions` table: survive restarts, end on logout, expire after `SESSION_TTL_HOURS` (24 h) |

## Endpoints

| Endpoint | Same | Changed (bug fixed) |
|---|---|---|
| `GET /health` | `200 {"status":"ok"}` | – |
| `POST /api/register` | 201 `{id, username}`; 400 missing field; 409 username or email taken; `name` defaults to the username | a non-string `password` gets 400 (legacy 500). Concurrent registrations can no longer create duplicate usernames |
| `POST /api/login` | 200 `{token}`; 401 `invalid credentials` | the token format (see above). Legacy MD5 password hashes still work and are upgraded to Argon2id on login |
| `POST /api/logout` | 200 `{"ok":true}` | **the token stops working** (legacy kept it valid) |
| `GET /api/users/me` | same fields | – |
| `PATCH /api/users/me` | updates `name`, `email`; returns the user | only `name`/`email` are applied and other keys are ignored. Legacy wrote any key into SQL, so `{"role":"admin"}` or `{"wallet_balance":…}` worked, and unknown keys gave 500. A blank or non-string value gets 400; an email used by another user gets 409 |
| `GET /api/products` | `q` (case-insensitive name search), `sort`, `page`, `limit` (default 20; negative = all) | `sort` outside `id, name, price, stock, sku` gets 400 (legacy put it straight into SQL: injection or 500). `page < 1` is treated as 1 (legacy: negative OFFSET, 500). `%` and `_` in `q` match literally. Ties are broken by id |
| `GET /api/products/{id}` | 200 / 404 | – |
| `POST /api/products` (admin) | 201 product | **401 without a token** (legacy let anonymous requests through). 403 for non-admins. A non-numeric or negative price or stock gets 400 (legacy silently stored 0) |
| `PUT /api/products/{id}` (admin) | omitted fields keep their value; 404 | same auth and validation fixes as POST |
| `POST /api/orders` | 201 order with items; 400 empty items, unknown or expired coupon; 404 unknown product; 409 out of stock or single-use coupon used; order numbers start at 1001 | `quantity < 1` gets 400 (legacy accepted negative quantities: negative totals, stock increased). `notify_url` must be http(s) (400). The whole order is atomic, so a failed order leaves nothing behind (legacy left a half-created order with stock reserved). No overselling or duplicate `order_no` under concurrency. A single-use coupon works exactly once even under concurrency |
| `GET /api/orders` | caller's orders, newest first, `page`/`limit` | `page < 1` is treated as 1 |
| `GET /api/orders/{id}` | 200 / 404 | only the **owner or an admin** can see it; others get 404 (legacy showed any order to any user) |
| `GET /api/orders/{id}/invoice` | `{order_no, customer, lines, total, status}` | same owner/admin rule; `line_total` rounded to cents |
| `POST /api/orders/{id}/pay` | `method` `wallet` (default) or `bank`; 200 `{order_id, status:"paid", method, bank_ref}`; 400 unknown method; 402 insufficient wallet; 404; 409 not pending; 502 bank failure | concurrent pays charge **once**. Bank retries reuse one `Idempotency-Key`, so there are no double charges (legacy retried blindly). The bank call times out (legacy waited forever) |
| `POST /api/orders/{id}/cancel` | returns stock, credits the wallet if paid; 404; 409 already cancelled | owner/admin only. The credit is `paid_amount − refunded`: legacy credited the full total even for refunded or never-paid ("shipped") orders |
| `POST /api/orders/{id}/refund` | default amount = what was paid; status becomes `refunded`; 409 while pending | owner/admin only. 409 when cancelled or nothing is left to refund; 400 for a non-positive amount or one above the refundable remainder. Legacy allowed unlimited and repeated refunds |
| `GET /api/wallet` | `{balance}` | – |
| `POST /api/wallet/topup` | `{balance}` | `amount` must be a number > 0 and ≤ 1e9 (400). Legacy accepted 0, negatives and garbage. Concurrent top-ups are never lost |
| `POST /api/wallet/transfer` | `{balance}` of the sender; 402; 404 recipient | a self-transfer gets 400 (legacy **created money**); a non-positive amount gets 400 (legacy: a negative transfer pulled money from the recipient); atomic |
| `GET /api/reports/sales` (admin) | `from` (inclusive), `to` (exclusive), `group_by` `day`/`status`/`user`; buckets and top 10 products from paid/shipped orders | **no stale cache** (legacy cached forever). `group_by` is whitelisted (legacy: SQL injection), and invalid dates get 400. Revenue is rounded to cents. 401 without a token |
| `POST /api/reports/export` (admin) | writes `<filename>.gz` (and the plain CSV) with header `order_no,date,status,user,product,qty,price`; default `sales.csv` | the filename must match `[A-Za-z0-9._-]` and not start with `.` (400). Legacy ran `gzip` through a **shell** with the user's filename. Gzip now runs in-process, and CSV fields are properly quoted |
| `GET /api/reports/download` (admin) | `application/gzip`; 404 missing | path traversal (`../`) gets 404 (legacy served any file the process could read) |
| `GET /api/admin/users` (admin) | the user list with `created_at` | `password_hash` removed from the response |
| `POST /api/admin/users/{id}/role` (admin) | default role `user`; 404 | `role` must be `user` or `admin` (400) |
| `POST /api/admin/orders/{id}/status` (admin) | sets the status; 404 | `status` must be one of `pending, paid, shipped, cancelled, refunded` (400). Legacy accepted any string or null |
| `POST /api/webhooks/bank` | HMAC-SHA256 hex of the raw body in `X-Signature`; `settled` marks the payment settled and moves a pending order to paid; 200 `{"ok":true}` | the signature is **required** (legacy accepted requests without the header) and compared in constant time. With a valid signature, malformed JSON gets 400. Moving to paid also records `paid_amount` and sends the `paid` notification to `notify_url` |

## Background behaviour

| Behaviour | Legacy | Rust |
|---|---|---|
| `notify_url` POSTs | `{order_id, order_no, status}` on creation and pay, in unordered threads | same body. Deliveries for one order are delivered in order (`pending` before `paid`), with 3 attempts and a 5 s timeout. The settlement webhook also triggers `paid` |
| E-mails | appended to an in-memory list nothing read | written to the log (`shopcore::mail`) |
| Audit log | every request body, passwords included, kept in memory forever | access log without bodies (`RUST_LOG=…,shopcore::http=debug`) |

## Maintenance commands (`manage.py`)

`shopcore <command>`; `python manage.py <command>` and `manage <command>` also work inside the image.

| Command | Same | Changed |
|---|---|---|
| `init-db` | creates the tables, prints `tables ready` | also applies the additive migrations 001 and 002 (README) |
| `seed` | admin (`ADMIN_USER`/`ADMIN_PASSWORD`), coupons `SAVE10` and `ONCE50`, sample products, each only if missing; prints `seeded` | admin password stored as Argon2id |
| `import-products <csv>` | `sku,name,price,stock` with a header; duplicate SKUs in the file keep the first; prints `imported N products` | parameterised SQL (legacy: string-formatted SQL injection). Proper CSV quoting. **Existing SKUs are updated** instead of duplicated. An invalid row fails the whole file with exit code 1 (legacy aborted halfway) |
| `export-sales <file>` | header `order_no,status,total,created_at`; prints `wrote <file> (N rows)` | CSV quoting |
| `purge-sessions` | logs everybody out | **actually works** (legacy cleared the memory of its own short-lived process, which did nothing). Prints `purged sessions (N)` |
| no or unknown command | usage message, exit 1 | same |
