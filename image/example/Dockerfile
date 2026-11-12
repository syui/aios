# aios の builder (ghcr.io/syui/aios) で Rust のプログラムを作り、aios-static に入れる (多段の docker build)
#   docker build -t hello image/example && docker run --rm hello
# CI (.github/workflows/image.yml) はこれを、その場で作った aios:ci と aios-static:ci でためす (--build-arg)
ARG AIOS=ghcr.io/syui/aios:latest
ARG AIOS_STATIC=ghcr.io/syui/aios-static:latest

FROM ${AIOS} AS builder
# ap は pacman と同じく、はじめに -y (データベース)。cargo は入れるときに Rust を static.rust-lang.org から取る
RUN ap -Sy cargo
WORKDIR /src
# ふつうは COPY . . (ここでは cargo init の Hello, world!)
RUN cargo init --vcs none --name hello .
# RUN はログインしないシェルなので、リンカの設定 (/etc/profile.d/cargo.sh) を読む。base-devel がなければ rust-lld で
# 静的なバイナリになる (aios-static で動く。base-devel の cc なら動的なので aios-cc に)
RUN . /etc/profile.d/cargo.sh && cargo build --release

FROM ${AIOS_STATIC}
COPY --from=builder /src/target/release/hello /usr/local/bin/hello
CMD ["/usr/local/bin/hello"]
