ARG AIOS=ghcr.io/syui/aios:0.0.1.r987.g13104fa@sha256:3f12715a3d0fe2baf7e796e88f4792852d2c9328025304b0438df1aa42de9dd0
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
