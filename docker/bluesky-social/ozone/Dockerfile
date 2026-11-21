ARG AIOS=ghcr.io/syui/aios:0.0.1.r1007.gb8a7aed@sha256:c415c2ac1b9f7a2c02186a3326d3a099687f2a54e6813bc484fa44d7a36b265a
ARG AIOS_NODE=ghcr.io/syui/aios-node:26.13.1@sha256:a0337d11bae69a2250392bfa8fc46f8447cc7a34445dfd25c8ba5428fbfedd3e

FROM ${AIOS} AS build
RUN ap -Sy --noconfirm nodejs pnpm base-devel python
WORKDIR /usr/src/ozone
COPY . .
RUN printf 'nodeLinker: hoisted\nallowBuilds:\n  esbuild: true\n  unrs-resolver: true\n  sharp: true\n  protobufjs: true\n  core-js: false\n  cypress: false\n' > pnpm-workspace.yaml
RUN node -e 'const fs=require("fs"),p=JSON.parse(fs.readFileSync("package.json"));delete p.packageManager;fs.writeFileSync("package.json",JSON.stringify(p,null,2)+"\n")' && pnpm import && pnpm install --frozen-lockfile
RUN pnpm build
RUN rm -rf node_modules .next/cache pnpm-lock.yaml yarn.lock && mv service/package.json package.json && mv service/yarn.lock yarn.lock
RUN node -e 'const fs=require("fs"),p=JSON.parse(fs.readFileSync("package.json"));delete p.packageManager;fs.writeFileSync("package.json",JSON.stringify(p,null,2)+"\n")' && pnpm import && pnpm install --prod --frozen-lockfile

FROM ${AIOS_NODE}
WORKDIR /usr/src/ozone
COPY --from=build --chown=65532:65532 /usr/src/ozone /usr/src/ozone
ENV TZ=Etc/UTC OZONE_PORT=3000 NODE_ENV=production
EXPOSE 3000
ENTRYPOINT []
CMD ["node", "./service"]
