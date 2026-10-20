#!/bin/zsh
set -e

d=${0:a:h:h}
source $d/.env

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

rm -rf "$WORK"
mkdir -p "$WORK"

cd "$WORK"
# pin main: the remote's default branch is `unix` (self-made kernel), which has no pkg/
git clone --depth 1 -b main https://git.syui.ai/ai/os.git

typeset -A PKG_REPOS
PKG_REPOS=(ailog log aigpt gpt aishell shell aifont font)

mkdir -p "$REPO_DIR/x86_64"

built_pkgs=()
failed_pkgs=()

for pkg in ailog aigpt aishell aifont; do
  repo_name="${PKG_REPOS[$pkg]}"
  remote_hash=$(git ls-remote --heads "https://git.syui.ai/ai/${repo_name}.git" main | cut -c1-7)
  existing=$(ls "$REPO_DIR/x86_64/${pkg}"-*.pkg.tar.zst 2>/dev/null | head -1)

  if [ -n "$existing" ] && echo "$existing" | grep -q "\.g${remote_hash}-"; then
    echo "=== $pkg unchanged (${remote_hash}), skipping ==="
    continue
  fi

  echo "=== Building $pkg (${remote_hash}) ==="
  cp -r "$WORK/os/pkg/$pkg" "$WORK/$pkg"
  # A single package's build failure must not block the others (nor the repo-add
  # + push that follows) — catch it, keep the tree around for a post-mortem, move on.
  if ! (cd "$WORK/$pkg" && makepkg -sf --noconfirm --sign --key "$GPG_KEY"); then
    echo "=== FAILED: $pkg — build tree kept at $WORK/$pkg on $HOST for inspection ==="
    failed_pkgs+=("$pkg")
    continue
  fi

  rm -f "$REPO_DIR/x86_64/${pkg}"-*.pkg.tar.zst
  rm -f "$REPO_DIR/x86_64/${pkg}"-*.pkg.tar.zst.sig
  rm -f "$REPO_DIR/x86_64/${pkg}-debug"-*.pkg.tar.zst
  rm -f "$REPO_DIR/x86_64/${pkg}-debug"-*.pkg.tar.zst.sig
  cp "$WORK"/"$pkg"/*.pkg.tar.zst "$REPO_DIR/x86_64/"
  cp "$WORK"/"$pkg"/*.pkg.tar.zst.sig "$REPO_DIR/x86_64/" 2>/dev/null || true
  built_pkgs+=("$pkg")
  rm -rf "$WORK/$pkg"
done

rm -rf "$WORK/os"
echo "=== Packages built: [${built_pkgs[*]}] ==="
if (( ${#failed_pkgs[@]} > 0 )); then
  echo "=== WARNING: ${#failed_pkgs[@]} package(s) failed and were skipped: [${failed_pkgs[*]}] — inspect \$HOME/aios-pkg/<pkg> on $HOST ==="
fi
REMOTE
}

function repo-kernel-preflight() {
  echo "=== Preflight check on $HOST_KERNEL ==="
  ssh "$HOST_KERNEL" zsh -s <<'REMOTE'
set -e
if pgrep -x pacman >/dev/null 2>&1; then
  echo "ERROR: pacman is currently running on $(hostname 2>/dev/null || cat /etc/hostname) — a concurrent system upgrade can corrupt the kernel build (shared libs replaced under a running parallel make, see aios-kernel-update-safety memory). Wait for it to finish and retry."
  exit 1
fi
if [ -f /var/lib/pacman/db.lck ]; then
  echo "ERROR: pacman lock file present (/var/lib/pacman/db.lck) — a transaction may be stuck. Investigate before building."
  exit 1
fi
echo "=== Preflight OK: no pacman activity ==="
REMOTE
}

function repo-kernel-patch() {
  echo "=== Patching linux-aios on $HOST_KERNEL ==="
  ssh "$HOST_KERNEL" zsh -s <<'REMOTE'
set -e
REPOS="${HOME}/ai"
WORK="${HOME}/aios-kernel"

mkdir -p "$REPOS"
if [ -d "$REPOS/archlinux" ]; then
  cd "$REPOS/archlinux"
  git pull
else
  git clone --depth 1 https://gitlab.archlinux.org/archlinux/packaging/packages/linux.git "$REPOS/archlinux"
fi

cd "$REPOS/archlinux"
upstream_rev=$(git rev-parse HEAD)
if [ -f "$WORK/linux-aios/.upstream-rev" ] && [ "$(cat "$WORK/linux-aios/.upstream-rev")" = "$upstream_rev" ]; then
  echo "=== upstream unchanged (${upstream_rev[1,7]}), reusing existing work tree (previous build likely failed for an unrelated/environmental reason) ==="
  exit 0
fi

rm -rf "$WORK"
mkdir -p "$WORK/linux-aios"
cp "$REPOS/archlinux/PKGBUILD" "$WORK/linux-aios/"
cp "$REPOS/archlinux/config.x86_64" "$WORK/linux-aios/"

# Apply aios-specific modifications directly (no patch round-trip).
cd "$WORK/linux-aios"
sed -i "1s/.*/# Maintainer: syui <syui@syui.ai>\n# Based on: Arch Linux linux package by Jan Alexander Steffens (heftig)/" PKGBUILD
sed -i "s/^pkgbase=linux$/pkgbase=linux-aios/" PKGBUILD
sed -i "s/^pkgdesc='Linux'$/pkgdesc='Linux (aios)'/" PKGBUILD
sed -i '/# htmldocs/,/texlive-latexextra/d' PKGBUILD
sed -i '/make htmldocs/d' PKGBUILD
sed -i '/_package-docs()/,/^}/d' PKGBUILD
sed -i '/"$pkgbase-docs"/d' PKGBUILD
sed -i '/^  echo "Setting version\.\.\."/d' PKGBUILD
sed -i '/echo "-$pkgrel" > localversion.10-pkgrel/d' PKGBUILD
sed -i '/echo "${pkgbase#linux}" > localversion.20-pkgname/d' PKGBUILD
sed -i '/^  done$/a\\n  echo "Setting version..."\n  sed -i "s/^EXTRAVERSION = .*/EXTRAVERSION =/" Makefile\n  echo "" > localversion.10-pkgrel\n  echo "-aios" > localversion.20-pkgname' PKGBUILD

echo "=== Patch result ==="
head -5 PKGBUILD
echo "--- prepare() version setting ---"
grep -A5 "^  done$" PKGBUILD | head -8

# sed はパターン不一致でも exit 0 = 黙って空振りする。適用結果を検証しドリフトを大声で落とす。
grep -q '^pkgbase=linux-aios$' PKGBUILD || { echo "PATCH DRIFT: pkgbase=linux-aios not set"; exit 1; }
grep -q 'localversion.20-pkgname' PKGBUILD || { echo "PATCH DRIFT: -aios localversion missing"; exit 1; }
if grep -q 'htmldocs' PKGBUILD; then echo "PATCH DRIFT: htmldocs not removed"; exit 1; fi
echo "$upstream_rev" > "$WORK/linux-aios/.upstream-rev"
echo "=== Patch verified (no drift) ==="
REMOTE
}

function repo-kernel-build() {
  echo "=== Building linux-aios on $HOST_KERNEL ==="
  ssh "$HOST_KERNEL" zsh -s <<'REMOTE'
set -e
cd "${HOME}/aios-kernel/linux-aios"
extract_flag=()
if [ -d src ] && [ -n "$(ls -A src 2>/dev/null)" ]; then
  echo "=== existing extracted src/ found (unchanged upstream), reusing with --noextract ==="
  extract_flag=(--noextract)
fi
makepkg -sf --noconfirm --skippgpcheck "${extract_flag[@]}"
echo "=== Kernel built ==="
REMOTE
}

function repo-kernel-transfer() {
  echo "=== Transferring kernel packages ==="
  tmpdir=$(mktemp -d)
  ssh "$HOST_KERNEL" "ls ~/aios-kernel/linux-aios/linux-aios-*.pkg.tar.zst" | while read f; do scp "${HOST_KERNEL}:$f" "$tmpdir/"; done

  setopt localoptions nullglob
  new_files=("$tmpdir"/linux-aios-*.pkg.tar.zst)
  if (( ${#new_files[@]} == 0 )); then
    echo "ERROR: no kernel packages retrieved from $HOST_KERNEL — repo left untouched"
    rm -rf "$tmpdir"
    exit 1
  fi

  # Upload alongside the existing packages first (as .new) and only swap them in
  # once every new file has actually landed — never leaves the repo without a
  # linux-aios package if a transfer fails partway.
  for f in "${new_files[@]}"; do
    scp "$f" "$HOST":~/ai/repo/x86_64/$(basename "$f").new
  done
  ssh "$HOST" "cd ~/ai/repo/x86_64 && setopt nonomatch 2>/dev/null; rm -f linux-aios-*.pkg.tar.zst linux-aios-*.pkg.tar.zst.sig; for f in *.new; do mv -- \"\$f\" \"\${f%.new}\"; done"

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

# Sign unsigned packages
for pkg in *.pkg.tar.zst; do
  [ -f "$pkg" ] || continue
  if [ ! -f "${pkg}.sig" ]; then
    echo "Signing $pkg..."
    gpg --detach-sign --default-key "$GPG_KEY" "$pkg"
  fi
done

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
# commit only when there are staged changes (avoids a "nothing to commit" abort).
# signing follows the repo-local config on the repo host
# (git config user.signingkey <ai key>; git config commit.gpgsign true).
# the old silent-drop bug was a masked commit failure via `... || true`.
if ! git diff --cached --quiet; then
  git commit -m "update $(date +%Y.%m.%d)"
fi
git push

echo "=== Done ==="
REMOTE
}

case "$1" in
  pkg)
    repo-pkg-build
    repo-db-update
    ;;
  kernel)
    repo-kernel-preflight
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
