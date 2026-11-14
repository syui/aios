ARG AIOS=ghcr.io/syui/aios:0.0.1.r987.g13104fa@sha256:3f12715a3d0fe2baf7e796e88f4792852d2c9328025304b0438df1aa42de9dd0
ARG AIOS_CC=ghcr.io/syui/aios-cc:1.2.6@sha256:df3a41a8d31e479662f8a2b6bd5e96f0274a839dd10e077d14513f8115da7be3

FROM ${AIOS} AS build
RUN ap -Sy --noconfirm cargo base-devel openssl && mkdir /empty
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN . /etc/profile.d/cargo.sh && OPENSSL_STATIC=1 cargo build --release --locked

FROM ${AIOS_CC}
COPY --from=build --chown=65532:65532 /empty /data
COPY --from=build /app/target/release/aichat /usr/local/bin/aichat
ENV DB_PATH=/data/chat.db PORT=3100
USER 65532:65532
VOLUME /data
EXPOSE 3100
ENTRYPOINT []
CMD ["/usr/local/bin/aichat"]
