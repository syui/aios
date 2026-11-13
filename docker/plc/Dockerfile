ARG AIOS=ghcr.io/syui/aios:0.0.1.r948.g3e76cb7@sha256:ddd41dd1a4e47b6c87eb9f30cd9dff2c76375cf9cadc7e42cd7f0fc78df784ee
ARG AIOS_NODE=ghcr.io/syui/aios-node:26.13.1@sha256:9767cd25ed8a4cce342ccdaf13199dadfccd14e5578935f88a5ce1ed6c5726ec

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
CMD ["--enable-source-maps", "--import=@did-plc/server/telemetry", "index.js"]
