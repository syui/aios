ARG AIOS=ghcr.io/syui/aios:0.0.1.r940.g4d08978@sha256:2ddf529312ed8da2f3116403e6d8294e528255e5020a9f398479901b36787856
ARG AIOS_STATIC=ghcr.io/syui/aios-static:20261105@sha256:6039d9e67630100421bcb1b1b0e769df59fe251ce34237b07896e3da9e878231

FROM ${AIOS} AS builder
RUN ap -Sy cargo
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN . /etc/profile.d/cargo.sh && cargo build --release --locked

FROM ${AIOS_STATIC}
COPY --from=builder /src/target/release/serve /usr/local/bin/serve
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/serve"]
