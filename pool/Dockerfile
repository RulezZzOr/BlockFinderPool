FROM rust:1.85-bookworm AS builder

ARG BUILD_GIT_SHA=unknown
ARG BUILD_GIT_DIRTY=unknown
ARG BUILD_SOURCE=docker
ARG BUILD_TIME_UTC=unknown
ARG BUILD_IMAGE_ID=unknown

ENV BUILD_GIT_SHA=${BUILD_GIT_SHA}
ENV BUILD_GIT_DIRTY=${BUILD_GIT_DIRTY}
ENV BUILD_SOURCE=${BUILD_SOURCE}
ENV BUILD_TIME_UTC=${BUILD_TIME_UTC}
ENV BUILD_IMAGE_ID=${BUILD_IMAGE_ID}

WORKDIR /app
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    libzmq3-dev \
    libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir -p src && echo "fn main() {}" > src/main.rs
RUN cargo build --release

COPY src ./src
RUN find src -name "*.rs" | xargs touch && cargo build --release

FROM debian:bookworm-slim
ARG BUILD_GIT_SHA=unknown
ARG BUILD_GIT_DIRTY=unknown
ARG BUILD_SOURCE=docker
ARG BUILD_TIME_UTC=unknown
ARG BUILD_IMAGE_ID=unknown

LABEL org.opencontainers.image.revision="${BUILD_GIT_SHA}"
LABEL org.opencontainers.image.created="${BUILD_TIME_UTC}"
LABEL org.opencontainers.image.version="${BUILD_SOURCE}"
LABEL blackhole.build.git_dirty="${BUILD_GIT_DIRTY}"
LABEL blackhole.build.image_id="${BUILD_IMAGE_ID}"

RUN apt-get update && apt-get install -y --no-install-recommends \
    libssl3 \
    libzmq5 \
    libsqlite3-0 \
    ca-certificates \
    netcat-openbsd \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/blackhole-pool /usr/local/bin/blackhole-pool

EXPOSE 2018 8080
ENTRYPOINT ["blackhole-pool"]
