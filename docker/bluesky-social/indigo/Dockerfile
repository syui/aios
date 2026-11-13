ARG AIOS=ghcr.io/syui/aios:0.0.1.r955.gfb57887@sha256:27e143fb3a8869a9832197b564e4d2d2b12556ef1c77ce029c90b36858c2e315
ARG AIOS_STATIC=ghcr.io/syui/aios-static:20261105@sha256:6039d9e67630100421bcb1b1b0e769df59fe251ce34237b07896e3da9e878231

FROM ${AIOS} AS build
RUN ap -Sy --noconfirm go && mkdir /empty
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 go build -trimpath -tags timetzdata -o /relay ./cmd/relay

FROM ${AIOS} AS web
RUN ap -Sy --noconfirm nodejs pnpm
WORKDIR /app
COPY cmd/relay/relay-admin-ui/ ./
RUN pnpm import && pnpm install --frozen-lockfile --config.node-linker=hoisted && pnpm run build

FROM ${AIOS_STATIC}
COPY --from=build --chown=65532:65532 /empty /data/relay
COPY --from=build /relay /relay
COPY --from=web /app/dist/ /public/
WORKDIR /
ENV GODEBUG=netdns=go TZ=Etc/UTC
USER 65532:65532
EXPOSE 2470
ENTRYPOINT []
CMD ["/relay", "serve"]
