FROM rust:1.88-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY backend/Cargo.toml backend/Cargo.toml
RUN mkdir -p backend/src && printf 'fn main() {}\n' > backend/src/main.rs
RUN cargo build --release --manifest-path backend/Cargo.toml
COPY backend backend
RUN touch backend/src/main.rs && cargo build --release --manifest-path backend/Cargo.toml

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install --no-install-recommends --yes ca-certificates wget \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home trefoil
COPY --from=builder /src/target/release/trefoil-backend /usr/local/bin/trefoil-backend
USER trefoil
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/trefoil-backend"]
