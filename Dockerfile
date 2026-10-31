# The image only wraps a binary that the release workflow has already built (one per
# architecture, in context/amd64 and context/arm64), so nothing is compiled here.
FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home closure

ARG TARGETARCH
COPY context/${TARGETARCH}/closure /usr/local/bin/closure

USER closure
# The admin API listens on 127.0.0.1 unless API_BIND says otherwise.
ENTRYPOINT ["/usr/local/bin/closure"]
