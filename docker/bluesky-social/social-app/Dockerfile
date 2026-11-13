ARG AIOS=ghcr.io/syui/aios:0.0.1.r955.gfb57887@sha256:27e143fb3a8869a9832197b564e4d2d2b12556ef1c77ce029c90b36858c2e315
ARG AIOS_STATIC=ghcr.io/syui/aios-static:20261105@sha256:6039d9e67630100421bcb1b1b0e769df59fe251ce34237b07896e3da9e878231

FROM ${AIOS} AS web-build
RUN ap -Sy --noconfirm nodejs base-devel python git && npm install -g pnpm@11.28.5
WORKDIR /app
ENV CI=1 NODE_OPTIONS=--max-old-space-size=8192
ARG RENDER_GIT_COMMIT
ARG EXPO_PUBLIC_ENV
ENV EXPO_PUBLIC_ENV=${EXPO_PUBLIC_ENV:-development}
ARG EXPO_PUBLIC_RELEASE_VERSION
ENV EXPO_PUBLIC_RELEASE_VERSION=$EXPO_PUBLIC_RELEASE_VERSION
ARG EXPO_PUBLIC_BUNDLE_IDENTIFIER
ENV EXPO_PUBLIC_BUNDLE_IDENTIFIER=${EXPO_PUBLIC_BUNDLE_IDENTIFIER:-$RENDER_GIT_COMMIT}
ARG SENTRY_AUTH_TOKEN
ENV SENTRY_AUTH_TOKEN=${SENTRY_AUTH_TOKEN:-unknown}
ARG EXPO_PUBLIC_SENTRY_DSN
ENV EXPO_PUBLIC_SENTRY_DSN=$EXPO_PUBLIC_SENTRY_DSN
COPY . .
RUN node -e 'const fs=require("fs"),p=JSON.parse(fs.readFileSync("package.json"));p.devEngines.runtime.onFail="warn";fs.writeFileSync("package.json",JSON.stringify(p,null,2)+"\n")'
RUN echo "EXPO_PUBLIC_ENV=$EXPO_PUBLIC_ENV" >> .env && \
  echo "EXPO_PUBLIC_RELEASE_VERSION=$EXPO_PUBLIC_RELEASE_VERSION" >> .env && \
  echo "EXPO_PUBLIC_BUNDLE_IDENTIFIER=$EXPO_PUBLIC_BUNDLE_IDENTIFIER" >> .env && \
  echo "EXPO_PUBLIC_BUNDLE_DATE=$(date -u +%y%m%d%H)" >> .env && \
  echo "EXPO_PUBLIC_SENTRY_DSN=$EXPO_PUBLIC_SENTRY_DSN" >> .env
RUN pnpm install --frozen-lockfile
RUN pnpm intl:build 2>&1 | tee i18n.log && ! grep -q "invalid syntax" i18n.log
RUN SENTRY_RELEASE="xyz.blueskyweb.app@${EXPO_PUBLIC_RELEASE_VERSION:-unset}" SENTRY_DIST=$EXPO_PUBLIC_BUNDLE_IDENTIFIER pnpm build-web

FROM ${AIOS} AS go-build
RUN ap -Sy --noconfirm go
WORKDIR /usr/src/social-app
ENV GODEBUG=netdns=go CGO_ENABLED=0
COPY --from=web-build /app/bskyweb ./bskyweb
RUN cd bskyweb && go mod download && go mod verify
RUN cd bskyweb && go build -trimpath -tags timetzdata -o /bskyweb ./cmd/bskyweb

FROM ${AIOS_STATIC}
ENV GODEBUG=netdns=go TZ=Etc/UTC
WORKDIR /bskyweb
COPY --from=go-build /bskyweb /usr/bin/bskyweb
USER 65532:65532
ENTRYPOINT []
CMD ["/usr/bin/bskyweb", "serve"]
