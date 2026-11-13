ARG AIOS=ghcr.io/syui/aios:0.0.1.r955.gfb57887@sha256:27e143fb3a8869a9832197b564e4d2d2b12556ef1c77ce029c90b36858c2e315
ARG AIOS_NODE=ghcr.io/syui/aios-node:26.13.1@sha256:a0337d11bae69a2250392bfa8fc46f8447cc7a34445dfd25c8ba5428fbfedd3e

FROM ${AIOS} AS build
RUN ap -Sy --noconfirm nodejs && npm install -g pnpm@11.11.0
WORKDIR /app
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml .npmrc tsconfig.json ./
COPY tsconfig ./tsconfig
COPY packages/lib ./packages/lib
COPY packages/server ./packages/server
RUN pnpm install --frozen-lockfile
RUN pnpm run build
RUN pnpm install --prod --shamefully-hoist --frozen-lockfile --prefer-offline --config.confirm-modules-purge=false

FROM ${AIOS_NODE}
COPY --from=build /app /app
WORKDIR /app/packages/server/service
USER 65532:65532
ENV NODE_ENV=production PORT=3000
EXPOSE 3000
ENTRYPOINT []
CMD ["node", "--enable-source-maps", "--import=@did-plc/server/telemetry", "index.js"]
