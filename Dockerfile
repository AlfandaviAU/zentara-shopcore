# ---- build ----
FROM rust:1.96-slim-bookworm AS builder
WORKDIR /build

# cache dependencies: build them once against stub sources
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src seed \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/.fingerprint/shopcore-* target/release/deps/shopcore-* target/release/deps/libshopcore-*

COPY src ./src
COPY seed ./seed
COPY tests ./tests
RUN cargo build --release --locked && strip target/release/shopcore

# ---- runtime ----
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home /srv/shopcore shopcore \
    && mkdir -p /srv/shopcore /tmp/exports \
    && chown shopcore /srv/shopcore /tmp/exports

COPY --from=builder /build/target/release/shopcore /usr/local/bin/shopcore
COPY seed /srv/shopcore/seed
COPY docker/manage.sh /usr/local/bin/manage
COPY docker/python-compat.sh /usr/local/bin/python
RUN chmod 755 /usr/local/bin/manage /usr/local/bin/python \
    && ln -s /usr/local/bin/manage /srv/shopcore/manage.py

WORKDIR /srv/shopcore
USER shopcore
ENV RUST_LOG=info,sqlx=warn
EXPOSE 8000
CMD ["shopcore", "serve"]
