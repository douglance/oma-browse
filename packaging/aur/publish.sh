#!/usr/bin/env bash
#
# Push one of the two AUR packages from this directory to the AUR.
#
#   packaging/aur/publish.sh oma-browse-bin
#   packaging/aur/publish.sh oma-browse-git
#
# The AUR is a git remote and nothing else: a repo per package, holding a
# PKGBUILD and a .SRCINFO and normally nothing more. The .SRCINFO is not a
# convenience -- it is what the AUR web interface and every helper read, and a
# push whose .SRCINFO disagrees with its PKGBUILD is rejected. So it is
# regenerated here rather than maintained by hand.
#
# For -bin the checksum is refreshed too, which downloads the release tarball:
# that is the point, since the checksum is the only thing standing between a
# user and a tarball that changed under the tag.
#
# One-time setup: an account on https://aur.archlinux.org with this machine's
# public key (~/.ssh/id_ed25519.pub) added under My Account -> SSH Public Key.
# Without it the push fails with "Permission denied (publickey)".

set -euo pipefail

pkg="${1:-}"
case "$pkg" in
  oma-browse-bin | oma-browse-git) ;;
  *)
    echo "usage: $0 {oma-browse-bin|oma-browse-git}" >&2
    exit 2
    ;;
esac

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
src="$here/$pkg"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Fail early and by name, rather than after a checksum download.
if ! ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
     aur@aur.archlinux.org help >/dev/null 2>&1; then
  echo "aur: ssh to aur@aur.archlinux.org was refused." >&2
  echo "     Add this key at https://aur.archlinux.org/account (My Account -> SSH Public Key):" >&2
  echo >&2
  sed 's/^/       /' ~/.ssh/id_ed25519.pub >&2
  exit 1
fi

# Clone if the package already exists on the AUR, start empty if it does not --
# a first submission is a push to a repo the server creates on demand.
if git clone --quiet "ssh://aur@aur.archlinux.org/$pkg.git" "$work/$pkg" 2>/dev/null &&
   [ -e "$work/$pkg/PKGBUILD" ]; then
  echo "aur: updating the existing $pkg"
else
  echo "aur: first submission of $pkg"
  rm -rf "$work/$pkg"
  git init --quiet -b master "$work/$pkg"
  git -C "$work/$pkg" remote add origin "ssh://aur@aur.archlinux.org/$pkg.git"
fi

cp "$src/PKGBUILD" "$work/$pkg/PKGBUILD"

cd "$work/$pkg"

# `sha256sums=('SKIP')` is right for a git source and wrong for a tarball, where
# it means "trust whatever downloads". Refresh it from the real file.
if [ "$pkg" = oma-browse-bin ]; then
  echo "aur: updpkgsums (downloads the release tarball)"
  updpkgsums
  grep -q "sha256sums=('SKIP')" PKGBUILD && {
    echo "aur: refused -- the tarball checksum is still SKIP" >&2; exit 1; }
fi

makepkg --printsrcinfo > .SRCINFO

# Keep the copy in the repo identical to what was published, so the next person
# to read packaging/aur/ is reading what is actually on the AUR.
cp PKGBUILD .SRCINFO "$src/"

git add PKGBUILD .SRCINFO
if git diff --cached --quiet; then
  echo "aur: nothing changed; not pushing"
  exit 0
fi

ver="$(awk -F= '/^pkgver=/{print $2}' PKGBUILD)"
git -c user.name="$(git -C "$here" config user.name)" \
    -c user.email="$(git -C "$here" config user.email)" \
    commit --quiet -m "$pkg $ver"
git push --quiet origin master
echo "aur: pushed https://aur.archlinux.org/packages/$pkg"
