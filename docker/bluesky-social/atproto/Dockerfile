# syntax=docker/dockerfile:1
ARG AIOS=ghcr.io/syui/aios:0.0.1.r955.gfb57887@sha256:27e143fb3a8869a9832197b564e4d2d2b12556ef1c77ce029c90b36858c2e315
ARG AIOS_NODE=ghcr.io/syui/aios-node:26.13.1@sha256:a0337d11bae69a2250392bfa8fc46f8447cc7a34445dfd25c8ba5428fbfedd3e

FROM ${AIOS} AS base
ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=0 PUPPETEER_SKIP_DOWNLOAD=true
RUN ap -Sy --noconfirm nodejs base-devel python && npm install -g corepack && corepack enable && mkdir /empty
WORKDIR /app
COPY package.json ./
RUN corepack install

FROM base AS deps
COPY .npmrc pnpm-lock.yaml pnpm-workspace.yaml ./
RUN pnpm fetch

FROM base AS source
COPY --chmod=755 <<'SH' /usr/local/bin/select-sources
#!/bin/sh
set -eu
mkdir /out
tar -cf - --exclude=./packages --exclude=./services . | tar -xf - -C /out
pnpm list --recursive --depth -1 --parseable --filter "$1..." | sed -n 's|^/app/||p' | tar -cf - -T - | tar -xf - -C /out
SH

FROM source AS bsky-source
RUN --mount=type=bind,target=/app select-sources bsky-app-view-service

FROM source AS bsync-source
RUN --mount=type=bind,target=/app select-sources bsync-service

FROM source AS ozone-source
RUN --mount=type=bind,target=/app select-sources ozone-service

FROM source AS pds-source
RUN --mount=type=bind,target=/app select-sources pds-service

FROM deps AS bsky-build
COPY --from=bsky-source /out/ ./
RUN pnpm install --offline --frozen-lockfile
RUN pnpm run build:tooling \
 && pnpm run --parallel --workspace-concurrency Infinity --filter-prod 'bsky-app-view-service...' prebuild \
 && pnpm exec tsc --build services/bsky/tsconfig.json \
 && pnpm --filter bsky-app-view-service --prod deploy /deploy \
 && rm /deploy/pnpm-lock.yaml /deploy/pnpm-workspace.yaml

FROM deps AS bsync-build
COPY --from=bsync-source /out/ ./
RUN pnpm install --offline --frozen-lockfile
RUN pnpm run --parallel --workspace-concurrency Infinity --filter-prod 'bsync-service...' prebuild \
 && pnpm exec tsc --build services/bsync/tsconfig.json \
 && pnpm --filter bsync-service --prod deploy /deploy \
 && rm /deploy/pnpm-lock.yaml /deploy/pnpm-workspace.yaml

FROM deps AS ozone-build
COPY --from=ozone-source /out/ ./
RUN pnpm install --offline --frozen-lockfile
RUN pnpm run build:tooling \
 && pnpm run --parallel --workspace-concurrency Infinity --filter-prod 'ozone-service...' prebuild \
 && pnpm exec tsc --build services/ozone/tsconfig.json \
 && pnpm --filter ozone-service --prod deploy /deploy \
 && rm /deploy/pnpm-lock.yaml /deploy/pnpm-workspace.yaml

FROM deps AS pds-build
COPY --from=pds-source /out/ ./
RUN pnpm install --offline --frozen-lockfile
RUN pnpm run build:tooling \
 && pnpm run --parallel --workspace-concurrency Infinity --filter-prod 'pds-service...' prebuild \
 && pnpm exec tsc --build services/pds/tsconfig.json \
 && pnpm run --parallel --filter-prod 'pds-service...' build:ui \
 && pnpm --filter pds-service --prod deploy /deploy \
 && rm /deploy/pnpm-lock.yaml /deploy/pnpm-workspace.yaml

FROM ${AIOS_NODE} AS runtime
ENV NODE_ENV=production
ENTRYPOINT []

FROM runtime AS bsky
WORKDIR /app/services/bsky
COPY --from=bsky-build /deploy /app/services/bsky
EXPOSE 3000
ENV PORT=3000
CMD ["node", "--heapsnapshot-signal=SIGUSR2", "--enable-source-maps", "--import=./tracer.ts", "--import=@atproto/bsky/telemetry", "api.ts"]

FROM runtime AS bsync
WORKDIR /app/services/bsync
COPY --from=bsync-build /deploy /app/services/bsync
EXPOSE 3000
ENV BSYNC_PORT=3000
CMD ["node", "--heapsnapshot-signal=SIGUSR2", "--enable-source-maps", "--import=@atproto/bsync/telemetry", "index.ts"]

FROM runtime AS ozone
WORKDIR /app/services/ozone
COPY --from=ozone-build /deploy /app/services/ozone
EXPOSE 3000 3001
ENV PORT=3000 OTEL_TRACES_SAMPLER=parentbased_traceidratio OTEL_TRACES_SAMPLER_ARG=0.01
CMD ["node", "--heapsnapshot-signal=SIGUSR2", "--enable-source-maps", "--import=./tracer.ts", "api.ts"]

FROM runtime AS pds
WORKDIR /app/services/pds
COPY --from=pds-build /deploy /app/services/pds
COPY --from=base --chown=65532:65532 /empty /app/data
VOLUME /app/data
EXPOSE 3000
ENV PDS_PORT=3000 UV_USE_IO_URING=0
CMD ["node", "--heapsnapshot-signal=SIGUSR2", "--enable-source-maps", "--import=@atproto/pds/telemetry", "index.ts"]
