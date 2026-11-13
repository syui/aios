# aios の builder で Rust のサーバー (serve) を作り、distroless の aios-static で動かす (多段の docker build)
#   docker compose -f docker/compose.yml up -d --build      → http://127.0.0.1:8080/
# image は digest で固める (タグは動くが、digest の中身は変わらない。aios の image は同じパッケージからいつも同じ
# digest になる)。新しくするときは digest を書きかえる: skopeo inspect docker://ghcr.io/syui/aios:latest | jq -r .Digest
ARG AIOS=ghcr.io/syui/aios:0.0.1.r940.g4d08978@sha256:2ddf529312ed8da2f3116403e6d8294e528255e5020a9f398479901b36787856
ARG AIOS_STATIC=ghcr.io/syui/aios-static:20261105@sha256:6039d9e67630100421bcb1b1b0e769df59fe251ce34237b07896e3da9e878231

FROM ${AIOS} AS builder
# pacman と同じく、はじめに -y (データベース)。cargo は入れるときに Rust を static.rust-lang.org から取る (sha256 を
# たしかめる)
RUN ap -Sy cargo
WORKDIR /src
# 要るものだけを写す (.dockerignore も)。Cargo.lock も (--locked で、そのとおりの依存だけ)
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# RUN はログインしないシェルなので、リンカの設定 (/etc/profile.d/cargo.sh) を読む。base-devel がないので rust-lld で
# 静的なバイナリになる (ローダーも libc も要らない: aios-static で動く。base-devel の cc を入れたら動的なので aios-cc に)
RUN . /etc/profile.d/cargo.sh && cargo build --release --locked

# 動かすほう: ユーザー (nonroot 65532)、CA 証明書、/tmp だけ。シェルも ap もない
FROM ${AIOS_STATIC}
COPY --from=builder /src/target/release/serve /usr/local/bin/serve
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/serve"]
