ARG AIOS=ghcr.io/syui/aios:0.0.1.r1007.gb8a7aed@sha256:c415c2ac1b9f7a2c02186a3326d3a099687f2a54e6813bc484fa44d7a36b265a
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
