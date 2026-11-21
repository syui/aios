ARG AIOS=ghcr.io/syui/aios:0.0.1.r1007.gb8a7aed@sha256:c415c2ac1b9f7a2c02186a3326d3a099687f2a54e6813bc484fa44d7a36b265a
ARG AIOS_CC=ghcr.io/syui/aios-cc:1.2.6@sha256:df3a41a8d31e479662f8a2b6bd5e96f0274a839dd10e077d14513f8115da7be3

FROM ${AIOS} AS build
RUN ap -Sy --noconfirm cargo base-devel openssl protobuf
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY build.rs ./
COPY proto ./proto
COPY src ./src
RUN . /etc/profile.d/cargo.sh && OPENSSL_STATIC=1 cargo build --release --locked

FROM ${AIOS_CC}
COPY --from=build /app/target/release/courier /usr/local/bin/courier
USER 65532:65532
EXPOSE 3300
ENTRYPOINT []
CMD ["/usr/local/bin/courier"]
