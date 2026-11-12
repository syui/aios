FROM ghcr.io/syui/aios AS builder
RUN ap -S cargo
WORKDIR /src
COPY . .
RUN cargo build --release

FROM ghcr.io/syui/aios-cc
COPY --from=builder /src/target/release/serve /usr/local/bin/serve
USER 65532:65532
CMD ["serve"]
