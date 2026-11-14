ARG AIOS=ghcr.io/syui/aios:0.0.1.r987.g13104fa@sha256:3f12715a3d0fe2baf7e796e88f4792852d2c9328025304b0438df1aa42de9dd0
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
