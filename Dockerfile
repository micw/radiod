FROM rust:1.99-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libgstreamer1.0-dev libasound2-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libasound2 gstreamer1.0-alsa gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/radiod /usr/local/bin/radiod
USER 65532:65532
EXPOSE 6600
ENTRYPOINT ["radiod"]
CMD ["--config", "/config/radiod.yaml", "--state", "/state/state.json"]
