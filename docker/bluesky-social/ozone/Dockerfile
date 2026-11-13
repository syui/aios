ARG AIOS=ghcr.io/syui/aios:0.0.1.r955.gfb57887@sha256:27e143fb3a8869a9832197b564e4d2d2b12556ef1c77ce029c90b36858c2e315
ARG AIOS_NODE=ghcr.io/syui/aios-node:26.13.1@sha256:a0337d11bae69a2250392bfa8fc46f8447cc7a34445dfd25c8ba5428fbfedd3e

FROM ${AIOS} AS build
ENV COREPACK_ENABLE_DOWNLOAD_PROMPT=0
RUN ap -Sy --noconfirm nodejs base-devel python && npm install -g corepack && corepack enable
WORKDIR /usr/src/ozone
COPY package.json yarn.lock .yarnrc.yml ./
RUN yarn
COPY . .
RUN yarn build
RUN rm -rf node_modules .next/cache
RUN mv service/package.json package.json && mv service/yarn.lock yarn.lock
RUN yarn

FROM ${AIOS_NODE}
WORKDIR /usr/src/ozone
COPY --from=build --chown=65532:65532 /usr/src/ozone /usr/src/ozone
ENV TZ=Etc/UTC OZONE_PORT=3000 NODE_ENV=production
EXPOSE 3000
ENTRYPOINT []
CMD ["node", "./service"]
