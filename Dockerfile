# syntax=docker/dockerfile:1.7
# Jellyfin with the iroh-host transport and the admin plugin, built from this repository's sources.
# Pinned versions and the reasons for them are in docs/versions.md.

FROM rust:1.99.1-slim-bookworm@sha256:24707e70a4f1e23d9a7b25202ec43126d28a52294b96aad5c670df1fd1f1d400 AS rust-build
# Overrides iroh/rust-toolchain.toml so the image's compiler is used without fetching clippy/rustfmt.
ENV RUSTUP_TOOLCHAIN=1.99.1 CARGO_TERM_COLOR=never
WORKDIR /src/iroh
COPY iroh/ ./
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/iroh/target \
    cargo build --release --locked -p iroh-host --bin iroh-host \
    && install -D -m 0755 target/release/iroh-host /out/iroh-host

FROM mcr.microsoft.com/dotnet/sdk:10.0.403@sha256:de4fe90a98845d75910b9769662465c84de1a934665c431279d1382b2bddc02b AS plugin-build
ENV DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
WORKDIR /src/jellyfin-plugin
COPY jellyfin-plugin/ ./
COPY docker/package-plugin.sh /usr/local/bin/package-plugin
RUN --mount=type=cache,target=/root/.nuget/packages \
    dotnet publish Jellyfin.Plugin.Iroh/Jellyfin.Plugin.Iroh.csproj -c Release -o /build \
    && sh /usr/local/bin/package-plugin /build meta.json /out

FROM jellyfin/jellyfin:12.4.20261026-211555@sha256:fea863d0cd9ec445985d9c75fc6ed64e38a1d2766cf0c250a33d553f4fc1b771
COPY --from=rust-build /out/iroh-host /usr/local/bin/iroh-host
COPY --from=plugin-build /out/ /opt/jellyfin-iroh/plugins/
COPY docker/entrypoint.sh /usr/local/bin/jellyfin-iroh-entrypoint
RUN chmod 0755 /usr/local/bin/jellyfin-iroh-entrypoint
ENV IROH_DATA_DIR=/iroh-data \
    IROH_ORIGIN=http://127.0.0.1:8096 \
    IROH_CONTROL_LISTEN=127.0.0.1:47950 \
    JELLYFIN_IROH_CONTROL_URL=http://127.0.0.1:47950 \
    JELLYFIN_IROH_CONTROL_TOKEN_FILE=/iroh-data/control.token
VOLUME ["/iroh-data"]
ENTRYPOINT ["/usr/local/bin/jellyfin-iroh-entrypoint"]
