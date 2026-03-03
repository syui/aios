#!/bin/zsh
set -e

d=${0:a:h:h}
source $d/.env

echo "=== Building and publishing packages on $HOST ==="
ssh "$HOST" zsh -s -- "$GPG_KEY" <<'REMOTE'
set -e
setopt nonomatch 2>/dev/null || true
GPG_KEY="$1"
REPO_NAME="aios"
REPO_DIR="${HOME}/ai/repo"
WORK="${HOME}/aios-pkg"

if [ ! -d "$REPO_DIR/.git" ]; then
  git clone git@git.syui.ai:ai/repo.git "$REPO_DIR"
  cd "$REPO_DIR"
  git config user.email $USER_EMAIL
  git config user.name $USER_NAME
fi

cd "$REPO_DIR"
git config user.signingkey "$GPG_KEY"
git config commit.gpgsign true

rm -rf "$WORK"
mkdir -p "$WORK"

cd "$WORK"
git clone --depth 1 https://git.syui.ai/ai/os.git

for pkg in ailog aigpt aishell; do
  echo "=== Building $pkg ==="
  cp -r "$WORK/os/pkg/$pkg" "$WORK/$pkg"
  cd "$WORK/$pkg"
  makepkg -sf --noconfirm --sign --key "$GPG_KEY"
  cd "$WORK"
done

mkdir -p "$REPO_DIR/x86_64"

for pkg in ailog aigpt aishell; do
  rm -f "$REPO_DIR/x86_64/${pkg}"-*.pkg.tar.zst
  rm -f "$REPO_DIR/x86_64/${pkg}"-*.pkg.tar.zst.sig
  rm -f "$REPO_DIR/x86_64/${pkg}-debug"-*.pkg.tar.zst
  rm -f "$REPO_DIR/x86_64/${pkg}-debug"-*.pkg.tar.zst.sig
  cp "$WORK"/"$pkg"/*.pkg.tar.zst "$REPO_DIR/x86_64/"
  cp "$WORK"/"$pkg"/*.pkg.tar.zst.sig "$REPO_DIR/x86_64/" 2>/dev/null || true
done

cd "$REPO_DIR/x86_64"
rm -f "${REPO_NAME}".{db,files}*
repo-add --sign --key "$GPG_KEY" "${REPO_NAME}.db.tar.gz" *.pkg.tar.zst
gpg --export "$GPG_KEY" > "$REPO_DIR/aios.gpg"

for f in "${REPO_NAME}.db" "${REPO_NAME}.files" "${REPO_NAME}.db.sig" "${REPO_NAME}.files.sig"; do
  if [ -L "$f" ]; then
    target=$(readlink "$f")
    rm "$f"
    cp "$target" "$f"
  fi
done
rm -f *.old *.old.sig

cd "$REPO_DIR"
git add -A
git commit -m "update $(date +%Y.%m.%d)" || true
git push

rm -rf "$WORK"

echo "=== Done ==="
REMOTE
