#!/bin/sh
# aios の上で atproto (ai/at の compose の中心) を動かす: aios の VM の中で動かす (test/vm.py start --kernel . --snapshot、
# AIOS_MEM=6G がよい。atproto の tsc は 2 GiB では足りない)。どれも aibox --image で、image は ghcr.io のもの。
#   sh at-aios.sh up     database (aios-postgres、ai/at と同じ init.sql の DB)、redis (aios-redis)、plc (did-method-plc を
#                        aios の上で pnpm でビルドし、aios-node で) を起こす
#   sh at-aios.sh pds    atproto (bluesky.yml と同じ commit) の pds-service を aios の上でビルドする (Dockerfile の
#                        pds-build と同じ手順。全体の pnpm install と better-sqlite3 のビルドで 30 分ほど)
#   sh at-aios.sh check  PDS を aios-node で起こし、アカウント (plc に DID を作る)、投稿、getRecord、listRecords、repo の
#                        CAR までたしかめる
# うしろで動かすときは < /dev/null で (端末を変えようとすると、Linux と同じく SIGTTOU で止まる)。ログは ~/at/*.log
export NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt
export PATH=$HOME/.corepack/bin:$HOME/.local/bin:$PATH
pw=atpw

up() {
  sudo ap -Syu --noconfirm >/dev/null 2>&1
  sudo ap -Sy --noconfirm nodejs git base-devel python >/dev/null 2>&1
  for i in aios-postgres:18.7 aios-redis:8.10.3 aios-node:latest; do ap image pull ghcr.io/syui/$i 2>&1 | tail -1; done
  mkdir -p ~/at/data/postgres ~/at/data/redis ~/at/configs/postgres/init ~/at/build
  printf 'CREATE DATABASE plc;\nCREATE DATABASE bgs;\nCREATE DATABASE carstore;\nCREATE DATABASE bsky;\n' > ~/at/configs/postgres/init/init.sql
  (aibox -b ~/at/data/postgres:/var/lib/postgresql -b ~/at/configs/postgres/init:/docker-entrypoint-initdb.d:ro \
    -e POSTGRES_USER=postgres -e POSTGRES_PASSWORD=$pw -e POSTGRES_DB=healthcheck \
    --image ghcr.io/syui/aios-postgres:18.7 > ~/at/pg.log 2>&1 &)
  (aibox -b ~/at/data/redis:/data --image ghcr.io/syui/aios-redis:8.10.3 > ~/at/redis.log 2>&1 &)
  [ -x ~/.local/bin/pnpm ] || npm install -g --prefix ~/.local pnpm@11.11.0 > ~/at/pnpm-i.log 2>&1
  cd ~/at/build
  if [ ! -d plc ]; then
    git clone -q https://github.com/did-method-plc/did-method-plc plc
    git -C plc checkout -q 29f1a70ee9f6f80a25946cac103b2f745e371334
  fi
  (cd plc && pnpm install --frozen-lockfile > ~/at/plc-install.log 2>&1 && pnpm run build > ~/at/plc-build.log 2>&1) || { echo "plc build failed"; exit 1; }
  for i in $(seq 1 30); do grep -q "init process complete" ~/at/pg.log && break; sleep 2; done
  creds='{"username":"postgres","password":"'$pw'","host":"127.0.0.1","port":"5432","database":"plc"}'
  (aibox -b ~/at/build/plc:/app:ro -e ENABLE_MIGRATIONS=true -e "DB_CREDS_JSON=$creds" -e "DB_MIGRATE_CREDS_JSON=$creds" \
    -e PORT=2582 -e NODE_ENV=production --image ghcr.io/syui/aios-node:latest \
    --enable-source-maps --import=/app/packages/server/dist/telemetry.js /app/packages/server/service/index.js > ~/at/plc.log 2>&1 &)
  for i in $(seq 1 30); do fetch http://127.0.0.1:2582/_health -o /dev/null >/dev/null 2>&1 && break; sleep 2; done
  fetch http://127.0.0.1:2582/_health -o /tmp/plc-health.json >/dev/null 2>&1 && cat /tmp/plc-health.json; echo
  echo "at-up: done"
}

pds() {
  set -e
  export COREPACK_ENABLE_DOWNLOAD_PROMPT=0 PUPPETEER_SKIP_DOWNLOAD=true
  [ -x ~/.corepack/bin/corepack ] || npm install -g --prefix ~/.corepack corepack > ~/at/pds-corepack.log 2>&1
  corepack enable --install-directory ~/.corepack/bin
  cd ~/at/build
  if [ ! -d atproto/.git ]; then
    git init -q atproto
    git -C atproto fetch -q --depth 1 https://github.com/bluesky-social/atproto 51c7e2e3657c8720b436351a9bc5559f386a8834
    git -C atproto checkout -q FETCH_HEAD
  fi
  cd atproto
  corepack install > ~/at/pds-corepack.log 2>&1
  echo "pnpm $(pnpm --version)"
  date
  pnpm install --frozen-lockfile > ~/at/pds-install.log 2>&1
  echo install ok; date
  pnpm run build:tooling > ~/at/pds-build.log 2>&1
  pnpm run --parallel --workspace-concurrency Infinity --filter-prod 'pds-service...' prebuild >> ~/at/pds-build.log 2>&1
  pnpm exec tsc --build services/pds/tsconfig.json >> ~/at/pds-build.log 2>&1
  pnpm run --parallel --filter-prod 'pds-service...' build:ui >> ~/at/pds-build.log 2>&1
  echo build ok; date
  rm -rf ~/at/build/pds-deploy
  pnpm --filter pds-service --prod deploy ~/at/build/pds-deploy > ~/at/pds-deploy.log 2>&1
  rm -f ~/at/build/pds-deploy/pnpm-lock.yaml ~/at/build/pds-deploy/pnpm-workspace.yaml
  echo deploy ok; date
  du -sh ~/at/build/pds-deploy
}

check() {
  mkdir -p ~/at/data/pds/blocks
  hex() { node -e 'console.log(require("crypto").randomBytes(32).toString("hex"))'; }
  [ -f ~/at/pds.env ] || {
    echo "PDS_JWT_SECRET=$(hex)"
    echo "PDS_ADMIN_PASSWORD=$(hex)"
    echo "PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX=$(hex)"
  } > ~/at/pds.env
  . ~/at/pds.env
  fetch http://127.0.0.1:2583/xrpc/_health -o /dev/null >/dev/null 2>&1 || (aibox -b ~/at/build/pds-deploy:/app/services/pds:ro -b ~/at/data/pds:/data \
    -e PDS_HOSTNAME=pds.test -e PDS_PORT=2583 -e PDS_DEV_MODE=true -e PDS_INVITE_REQUIRED=false \
    -e PDS_SERVICE_HANDLE_DOMAINS=.test -e PDS_DATA_DIRECTORY=/data -e PDS_BLOBSTORE_DISK_LOCATION=/data/blocks \
    -e PDS_DID_PLC_URL=http://127.0.0.1:2582 -e "PDS_JWT_SECRET=$PDS_JWT_SECRET" -e "PDS_ADMIN_PASSWORD=$PDS_ADMIN_PASSWORD" -e "PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX=$PDS_PLC_ROTATION_KEY_K256_PRIVATE_KEY_HEX" \
    -e LOG_ENABLED=true -e NODE_ENV=production -e UV_USE_IO_URING=0 \
    --image ghcr.io/syui/aios-node:latest --enable-source-maps /app/services/pds/index.ts > ~/at/pds-server.log 2>&1 < /dev/null &)
  for i in $(seq 1 90); do fetch http://127.0.0.1:2583/xrpc/_health -o /tmp/pds-h.json >/dev/null 2>&1 && break; sleep 2; done
  cat /tmp/pds-h.json 2>/dev/null; echo
  node --input-type=module -e '
  const base = "http://127.0.0.1:2583/xrpc/";
  const call = async (m, body, jwt, get) => {
    const r = await fetch(base + m + (get ? "?" + new URLSearchParams(body) : ""), get ? { headers: jwt ? { authorization: "Bearer " + jwt } : {} } : {
      method: "POST", headers: { "content-type": "application/json", ...(jwt ? { authorization: "Bearer " + jwt } : {}) }, body: JSON.stringify(body) });
    const t = r.headers.get("content-type")?.includes("json") ? await r.json() : new Uint8Array(await r.arrayBuffer());
    if (!r.ok) throw new Error(m + " " + r.status + " " + JSON.stringify(t));
    return t;
  };
  const pw = "aios-" + Math.random().toString(36).slice(2);
  const a = await call("com.atproto.server.createAccount", { handle: "aios.test", email: "aios@pds.test", password: pw });
  console.log("account", a.handle, a.did);
  const plc = await (await fetch("http://127.0.0.1:2582/" + a.did)).json();
  console.log("plc doc", plc.alsoKnownAs[0], plc.service[0].serviceEndpoint);
  const s = await call("com.atproto.server.createSession", { identifier: "aios.test", password: pw });
  const p = await call("com.atproto.repo.createRecord", { repo: a.did, collection: "app.bsky.feed.post", record: { $type: "app.bsky.feed.post", text: "hello from aios", createdAt: new Date().toISOString() } }, s.accessJwt);
  console.log("post", p.uri);
  const g = await call("com.atproto.repo.getRecord", { repo: a.did, collection: "app.bsky.feed.post", rkey: p.uri.split("/").pop() }, null, true);
  console.log("get", g.value.text);
  const car = await call("com.atproto.sync.getRepo", { did: a.did }, null, true);
  console.log("car bytes", car.length > 0);
  const l = await call("com.atproto.repo.listRecords", { repo: a.did, collection: "app.bsky.feed.post" }, null, true);
  console.log("list", l.records.length, l.records[0].value.text);
  ' 2>&1
  tail -3 ~/at/pds-server.log
}

case $1 in up | pds | check) "$1" ;; *) sed -n 2,11p "$0" >&2; exit 2 ;; esac
