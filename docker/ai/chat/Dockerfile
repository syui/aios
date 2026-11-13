ARG AIOS=ghcr.io/syui/aios:0.0.1.r955.gfb57887@sha256:27e143fb3a8869a9832197b564e4d2d2b12556ef1c77ce029c90b36858c2e315
ARG AIOS_STATIC=ghcr.io/syui/aios-static:20261105@sha256:6039d9e67630100421bcb1b1b0e769df59fe251ce34237b07896e3da9e878231

FROM ${AIOS} AS build
RUN ap -Sy --noconfirm cargo base-devel openssl && mkdir /empty
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM ${AIOS_STATIC}
COPY --from=build --chown=65532:65532 /empty /data
COPY --from=build /app/target/release/aichat /usr/local/bin/aichat
ENV DB_PATH=/data/chat.db PORT=3100
USER 65532:65532
VOLUME /data
EXPOSE 3100
ENTRYPOINT []
CMD ["/usr/local/bin/aichat"]
