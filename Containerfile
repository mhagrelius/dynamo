# dynamo.
#
#     podman build -f Containerfile -t dynamo .
#
# At the repo root rather than under `server/`, unlike brain, planner and
# armory: those repos are a GTK application whose server is one crate of
# several, so the server needs a folder to be distinguished from the shell.
# This repo is only the service, so there is nothing to distinguish it from.
#
# See `.dockerignore` — without it this ships `target/`.

FROM docker.io/library/rust:1-slim-bookworm AS build

WORKDIR /build

# No apt in the build stage, which is worth stating because every sibling needs
# one. armory installs libsqlite3-dev because rusqlite links the system sqlite;
# this takes Postgres over the wire through a pure-Rust client, and TLS through
# rustls, so there is no C library to link and nothing to install.

COPY Cargo.toml Cargo.lock ./
COPY src ./src

# --locked so the image is built from the versions that were tested, and fails
# loudly if the lockfile and the manifest have drifted rather than quietly
# resolving something newer.
RUN cargo build --release --locked

FROM docker.io/library/debian:bookworm-slim

# **No ca-certificates, and that is deliberate rather than an oversight.**
#
# This talks TLS to two public hosts, so the obvious reading is that it needs a
# trust store. It does not: `ureq`'s `tls` feature resolves to rustls with
# `webpki-roots`, which compiles Mozilla's root set into the binary. Checked
# with `cargo tree`, not assumed — `rustls-native-certs` and OpenSSL are both
# absent from the tree.
#
# The consequence worth knowing: root certificates are updated by rebuilding
# this image, not by `apt upgrade` on the host. That is the same bargain every
# static binary makes and it is the reason the runtime stage installs nothing
# at all.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libgcc-s1 \
 && rm -rf /var/lib/apt/lists/*

# A non-root user, which is right on an ordinary Docker host. On the Synology
# it is overridden back to root in the compose file — /volume1 carries ACLs
# that beat POSIX ownership. This container writes nothing to disk at all, so
# the override matters less here than for a service with a volume; the compose
# file has the whole story and keeps the line anyway.
RUN useradd --system --uid 10001 --no-create-home dynamo

COPY --from=build /build/target/release/dynamo /usr/local/bin/dynamo

USER 10001:10001

# **No EXPOSE, because nothing listens.** Every sibling server publishes a port
# and protects it with a bearer token. This one is a client: it opens outbound
# connections to Cognito and to the Emporia API, writes to Postgres, and offers
# no way in. There is no port to forward, no token to leak and no surface.

# The image has no curl, and there is no port for one to call anyway. `--health`
# asks Postgres instead: it reports unwell when the database is unreachable,
# when no pass has succeeded recently, or when the refresh token has been
# rejected — which are the three ways this stops working.
HEALTHCHECK --interval=60s --timeout=10s --start-period=120s --retries=3 \
    CMD ["/usr/local/bin/dynamo", "--health"]

ENTRYPOINT ["/usr/local/bin/dynamo"]
