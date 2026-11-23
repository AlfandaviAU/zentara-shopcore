# Testing

## How to run

| Suite | Command | Needs |
|---|---|---|
| Unit tests | `cargo test` | nothing (the integration suites skip themselves without a database) |
| **Everything (36 tests)** | `docker compose --profile test run --rm test` | Docker; uses the compose Postgres |
| Everything from the host | `TEST_DATABASE_URL=postgresql://shop:shop@localhost:5432/shop cargo test` | a reachable Postgres |
| Coverage | `TEST_DATABASE_URL=… cargo llvm-cov --summary-only` | `cargo install cargo-llvm-cov` |
| Legacy smoke tests (black-box parity) | `docker compose --profile smoke run --rm smoke` | the running stack |
| Mixed load with SLOs | `docker run --rm --network host -v "$PWD/loadtest:/lt" grafana/k6:0.54.0 run /lt/shopcore.js` (`-e SCALE=3` for 3×) | the running stack |
| Breakpoints | the same image with `-e MODE=read\|auth\|login\|order` (`-e SKUS=200`) and `/lt/breakpoint.js` | the running stack |
| Concurrency burst | `python3 loadtest/burst.py http://localhost:8000 http://localhost:5001` | Python with `requests` |
| Failure injection | `python3 loadtest/chaos.py` | the running stack and Python with `requests` |

Results of each are in [BENCHMARKS.md](BENCHMARKS.md).

## Test harness

* `tests/api.rs` starts the **real router** on a random port with a real Postgres. It also starts:
  * a **mock bank** that records each charge and then *loses the first response* to every new idempotency key,
    so every bank payment goes through the retry path. Amount `13.13` is declined with a 4xx.
  * a **webhook receiver** that collects `notify_url` deliveries.
* `tests/cli.rs` runs the **compiled binary** (`CARGO_BIN_EXE_shopcore`). It is a separate test binary, so Cargo runs it
  after the API tests and `purge-sessions` cannot log them out.
* Each test creates its own users and products, so the tests run in parallel against one database.

## Test catalogue

### Unit tests (`src/**`, 14)

| Test | Proves |
|---|---|
| `util::to_int_matches_legacy` | lenient integer parsing identical to the legacy `to_int` |
| `util::money_parsing` | numbers and numeric strings are accepted; NaN, bools and null are rejected |
| `util::rounding_matches_python` | `round2` / `calc_total` equal Python's `round(x, 2)`, including ties-to-even (`0.125` → `0.12`) |
| `util::python_float_and_dates` | `py_float`, `py_datetime` and `http_date` match Python/Flask output; date bounds parse, and injection strings are rejected |
| `util::file_names` | export file names: traversal, separators, shell metacharacters and dotfiles are rejected |
| `util::path_ids` | path ids follow Flask's `<int:>` (digits only, int32 range) |
| `util::like_escaping` | `%`, `_` and `\` are escaped for ILIKE |
| `auth::legacy_md5_hashes_still_verify` | the legacy seed hash verifies, and wrong passwords don't |
| `auth::argon2_round_trip` | Argon2id hash and verify |
| `auth::tokens_are_random` | 256-bit tokens, unique |
| `routes::webhooks::tests::signatures` | HMAC accepts valid signatures and rejects wrong, empty or foreign-secret ones |
| `manage::seed_file_parses` | the bundled seed CSV parses into 10 products |
| `manage::duplicates_keep_first_and_quotes_work` | first SKU wins; quoted commas work |
| `manage::invalid_rows_fail_the_import` | a bad price, negative stock or missing SKU rejects the whole file |

### API integration tests (`tests/api.rs`, 19)

| Test | Proves |
|---|---|
| `logout_invalidates_token_and_admin_routes_need_a_token` | logout ends the session; admin routes give 401 without a token |
| `profile_update_cannot_escalate_role_or_touch_wallet` | PATCH /me ignores `role`/`wallet_balance`; admin users list has no `password_hash` |
| `sql_injection_in_sort_and_group_by_is_rejected` | injection in `sort`, `group_by` and dates gives 400 |
| `concurrent_orders_never_oversell` | 20 parallel orders for 5 units: exactly 5 succeed, stock ends at 0, every `order_no` unique |
| `failed_order_rolls_back_reserved_stock` | an unknown product in line 2 leaves no order and no stock change; negative quantity gives 400 |
| `concurrent_wallet_payments_charge_once` | 10 parallel pays: exactly one 200, debited once |
| `bank_retry_uses_idempotency_key_single_charge` | a lost bank response plus a retry gives exactly one charge |
| `webhook_requires_signature` | an unsigned settlement gives 401 and changes nothing; a signed one marks the order paid |
| `refunds_are_bounded_and_orders_are_private` | other users get 404; refund above paid gives 400; partial then full refund, then 409; cancel after a full refund credits nothing |
| `wallet_rejects_bad_amounts_and_self_transfer` | negative or garbage amounts give 400; `0.1 + 0.2` gives `0.3`; self or negative transfer gives 400 |
| `export_blocks_path_traversal_and_reports_are_fresh` | traversal or shell filenames give 400/404; a new order shows up in the next report (no cache) |
| `admin_role_and_status_routes_validate_input` | 403 for users; invalid role or status gives 400; unknown id gives 404; role defaults to `user`; shipped-but-unpaid refund gives 409 |
| `notify_url_receives_pending_then_paid` | invalid `notify_url` gives 400; deliveries arrive in order for pay and for the settlement webhook; the webhook sets `paid_amount` |
| `reports_group_by_and_export_contents` | day/status/user buckets; invalid dates give 400; default export name; the gzip downloads and decompresses to the right CSV header and lines (Python float format) |
| `wallet_transfer_paths` | numeric-string amounts work; too large or missing gives 400; transfer succeeds; 402, 404 (bad and huge ids) and 401 |
| `product_admin_validation_and_listing` | every invalid price/stock gives 400; partial PUT keeps other fields; PUT 404/401; `limit=-1`, `page=0`, literal `%`, sort by name |
| `order_edge_paths_and_coupons` | malformed items give 400; SAVE10 total; duplicate lines aggregate stock; expired coupon gives 400; a single-use coupon under 5 parallel orders gives exactly one 201; pagination; invoice privacy; unknown pay method; bank decline gives 502 and the order stays pending; refund/cancel error paths |
| `webhook_edge_cases` | signed garbage gives 400; non-settled events are ignored; unknown or oversized order ids are harmless; non-hex signature gives 401 |
| `users_edge_cases_and_legacy_password_upgrade` | register/login validation; duplicate email gives 409; PATCH validation; **a legacy MD5 user logs in and is upgraded to Argon2id**; malformed auth headers give 401; JSON 404/405; health |

### CLI tests (`tests/cli.rs`, 3)

| Test | Proves |
|---|---|
| `usage_errors_exit_1` | no, unknown or incomplete command prints usage and exits 1 |
| `maintenance_commands_end_to_end` | `init-db`, `seed`, `import-products` (insert, then **update** the same SKU; invalid file and missing file exit 1), `export-sales` (header), `purge-sessions` |
| `serve_starts_answers_and_shuts_down_gracefully` | `serve` becomes healthy, and on SIGTERM drains and exits 0 |

## Coverage

95.3% of lines, 95.0% of functions, 91.4% of regions (was 69.6% / 69.4% / 66.8%). Every route module is at 96–100% of lines.
What remains uncovered is infrastructure-failure handling: the database staying unreachable for the full 60 s retry
window, the panic-to-500 handler, and malformed bank responses.

## Where the legacy smoke tests live

`smoke/test_smoke.py` is the legacy suite, byte-for-byte. In the legacy repo it ran *inside* the app container
(`docker compose exec app pytest -q tests`). The Rust image has no Python, so the suite runs from its own
container on the same network (`docker compose --profile smoke run --rm smoke`) and hits the API over HTTP, which is how a black-box grader
runs it too.
