# Debian's bundled libsoup 3.2/3.6 fail some HTTP/2 radio redirects.
# Build the unmodified fixed upstream release; verify and ship its exact source.
FROM debian:trixie-slim AS soup-build
RUN apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config meson ninja-build curl ca-certificates libglib2.0-dev libnghttp2-dev libpsl-dev libsqlite3-dev glib-networking && rm -rf /var/lib/apt/lists/*
WORKDIR /build
RUN curl --fail --location --retry 3 -o libsoup.tar.xz https://download.gnome.org/sources/libsoup/3.8/libsoup-3.8.0.tar.xz \
    && echo 'bbf08fa3e03a88c31a3d27a0d87cb422e9490f2d08e149211103df6d638a2238  libsoup.tar.xz' | sha256sum -c - \
    && tar -xf libsoup.tar.xz
RUN meson setup build libsoup-3.8.0 --prefix=/usr/local --libdir=lib --buildtype=release --default-library=shared --wrap-mode=nodownload -Dintrospection=disabled -Dvapi=disabled -Ddocs=disabled -Dtests=false -Dinstalled_tests=false -Dsysprof=disabled -Dgssapi=disabled -Dntlm=disabled -Dbrotli=disabled -Dzstd=disabled \
    && meson compile -C build && DESTDIR=/out meson install -C build

FROM rust:1.99-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libgstreamer1.0-dev libasound2-dev gstreamer1.0-plugins-base gstreamer1.0-plugins-good && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo test --locked && cargo build --release --locked

FROM debian:trixie-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates glib-networking libasound2t64 libsqlite3-0 libpsl5t64 libnghttp2-14 gstreamer1.0-alsa gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav && rm -rf /var/lib/apt/lists/*
COPY --from=soup-build /out/usr/local/lib/libsoup-3.0.so.0.8.0 /usr/local/lib/
COPY --from=soup-build /build/libsoup-3.8.0/COPYING /usr/share/radiod/third-party/libsoup-COPYING
COPY --from=soup-build /build/libsoup.tar.xz /usr/share/radiod/third-party/libsoup-3.8.0.tar.xz
RUN ldconfig
COPY --from=build /build/target/release/radiod /usr/local/bin/radiod
LABEL io.radiod.libsoup.version="3.8.0"
ENV XDG_CACHE_HOME=/tmp/cache GST_REGISTRY=/tmp/gstreamer-registry.bin GSETTINGS_BACKEND=memory
USER 65532:65532
EXPOSE 6600
ENTRYPOINT ["radiod"]
CMD ["--config", "/config/radiod.yaml", "--state", "/state/state.json"]

# Exercise the actual release binary and native runtime, not only the build host.
FROM runtime AS test
USER 0
RUN apt-get update && apt-get install -y --no-install-recommends python3 && rm -rf /var/lib/apt/lists/*
COPY tests /tests
COPY examples /examples
RUN chmod -R a+rX /tests /examples
ENV RADIOD_BINARY=/usr/local/bin/radiod RADIOD_TRACE_AUDIO=1
USER 65532:65532
RUN python3 -c 'import ctypes; s=ctypes.CDLL("libsoup-3.0.so.0"); v=tuple(getattr(s,"soup_get_"+n+"_version")() for n in ("major","minor","micro")); print("loaded libsoup:",v); assert v == (3,8,0)' \
    && radiod --config /examples/radiod.yaml --check-config \
    && python3 /tests/smoke.py && python3 /tests/streaming.py \
    && python3 -m unittest discover -s /tests -p 'test_*.py' \
    && printf 'release-runtime tests passed\n' > /tmp/radiod-test-passed

FROM runtime
COPY --from=test /tmp/radiod-test-passed /usr/share/radiod/test-passed
