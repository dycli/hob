# hob as a container image: a static binary on a minimal base.
#
#   docker build -t hob .
#   docker run -p 8970:8970 -e HOB_TZ=Europe/London -e HOB_WEATHER=51.48,-0.01 hob
#
# Configure it with HOB_* variables (see the README). Mount secret files
# (the Immich key, CalDAV accounts) read-only and point HOB_IMMICH_KEY_FILE
# and HOB_CALDAV_FILE at them. Fitted photos are kept in /var/cache/hob;
# give it a volume so they survive a restart.

# Build: a fully static binary against musl. TLS roots are compiled in
# (webpki-roots) and the time zone database too (chrono-tz), so the runtime
# image needs neither.
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
# Dependencies first, so a source change rebuilds only hob itself.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY src ./src
# COPY keeps the files' own times, older than the placeholder build above;
# touching main.rs makes cargo rebuild hob rather than keep the placeholder.
RUN touch src/main.rs && cargo build --release --locked

# Run: distroless static, as its non-root user.
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=build /src/target/release/hob /usr/local/bin/hob
ENV HOB_LISTEN=0.0.0.0:8970 \
    CACHE_DIRECTORY=/var/cache/hob
# Created owned by the image's user, so a fresh named volume is writable.
COPY --from=build --chown=nonroot:nonroot /tmp /var/cache/hob
VOLUME /var/cache/hob
EXPOSE 8970
USER nonroot
ENTRYPOINT ["/usr/local/bin/hob"]
