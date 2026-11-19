# The `moul` binary in a small image, for `moul hub` and `moul serve`.
# Build from the repository root:
#   docker build -f deploy/sync/Dockerfile -t moul .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
# rust-toolchain.toml asks for the browser target too; the binary needs only
# the native one.
RUN rustup show && cargo build --release --locked -p moul-cli

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /data --shell /usr/sbin/nologin moul \
 && mkdir -p /data && chown moul /data
COPY --from=build /src/target/release/moul /usr/local/bin/moul
USER moul
VOLUME /data
EXPOSE 8080
# The hub: its directory on the volume, replicas in the bucket that
# MOUL_STORE names (credentials from AWS_ACCESS_KEY_ID and
# AWS_SECRET_ACCESS_KEY), sign-up by invitation.
ENTRYPOINT ["/bin/sh", "-c", "exec moul hub /data --listen 0.0.0.0:8080 --public-url \"$MOUL_PUBLIC_URL\" --store \"$MOUL_STORE\" --signup \"${MOUL_SIGNUP:-invite}\""]
