#!/bin/zsh
set -e

d=${0:a:h:h}
source $d/.env

function repo-env() {
  REPO_NAME="aios"
  GPG_KEY="$GPG_KEY"
}

function repo-pkg-build() {
  echo "=== Building packages on $HOST ==="
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

rm -rf "$WORK"
echo "=== Packages built ==="
REMOTE
}

function repo-kernel-patch() {
  echo "=== Patching linux-aios on $HOST_KERNEL ==="
  ssh "$HOST_KERNEL" zsh -s <<'REMOTE'
set -e
REPOS="${HOME}/repos"
WORK="${HOME}/aios-kernel"

mkdir -p "$REPOS"
if [ -d "$REPOS/archlinux" ]; then
  cd "$REPOS/archlinux"
  git pull
else
  git clone --depth 1 https://gitlab.archlinux.org/archlinux/packaging/packages/linux.git "$REPOS/archlinux"
fi

rm -rf "$WORK"
mkdir -p "$WORK/linux-aios"
cp "$REPOS/archlinux/PKGBUILD" "$WORK/linux-aios/"
cp "$REPOS/archlinux/config.x86_64" "$WORK/linux-aios/"

cd "$WORK"
git clone --depth 1 https://git.syui.ai/ai/os.git
cd "$WORK/linux-aios"
patch -p1 < "$WORK/os/pkg/linux-aios/aios.patch"

echo "=== Patch result ==="
head -5 PKGBUILD
echo "--- prepare() version setting ---"
grep -A5 "^  done$" PKGBUILD | head -8
REMOTE
}

function repo-kernel-build() {
  echo "=== Building linux-aios on $HOST_KERNEL ==="
  ssh "$HOST_KERNEL" zsh -s <<'REMOTE'
set -e
cd "${HOME}/aios-kernel/linux-aios"
makepkg -sf --noconfirm --skippgpcheck
echo "=== Kernel built ==="
REMOTE
}

function repo-kernel-transfer() {
  echo "=== Transferring kernel packages ==="
  tmpdir=$(mktemp -d)
  ssh "$HOST_KERNEL" "ls ~/aios-kernel/linux-aios/linux-aios-*.pkg.tar.zst" | while read f; do scp "${HOST_KERNEL}:$f" "$tmpdir/"; done
  scp "$tmpdir"/linux-aios-*.pkg.tar.zst "$HOST":~/ai/repo/x86_64/
  rm -rf "$tmpdir"
  ssh "$HOST_KERNEL" "rm -rf ~/aios-kernel"
}

function repo-db-update() {
  echo "=== Updating repo database ==="
  ssh "$HOST" zsh -s -- "$GPG_KEY" <<'REMOTE'
set -e
setopt nonomatch 2>/dev/null || true
GPG_KEY="$1"
REPO_NAME="aios"
REPO_DIR="${HOME}/ai/repo"

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

echo "=== Done ==="
REMOTE
}

repo-env
case "$1" in
  pkg)
    repo-pkg-build
    repo-db-update
    ;;
  kernel)
    repo-kernel-patch
    repo-kernel-build
    repo-kernel-transfer
    repo-db-update
    ;;
  kernel-test)
    repo-kernel-patch
    ;;
  *)
    repo-pkg-build
    repo-db-update
    ;;
esac
