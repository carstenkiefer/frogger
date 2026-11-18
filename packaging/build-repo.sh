#!/usr/bin/env bash
# Build a signed APT and DNF package repository from release packages.
#
#   packaging/build-repo.sh <dir with .deb/.rpm files> <out-dir> [base-url]
#
# The signing key must be the only secret key in the GnuPG keyring
# (GNUPGHOME). Layout of <out-dir>:
#
#   frogger.asc                       public signing key
#   apt/dists/stable/…, apt/pool/…    APT repository (all architectures)
#   rpm/repodata/…, rpm/*.rpm         DNF repository (metadata signed)
#   frogger.repo                      DNF repository definition
#
# Needs: apt-ftparchive (apt-utils), createrepo_c, gpg.
set -euo pipefail
IN=$(cd "${1:?package directory}" && pwd)
OUT=${2:?output directory}
BASE=${3:-https://carstenkiefer.github.io/frogger}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)

key=$(gpg --batch --list-secret-keys --with-colons | awk -F: '$1=="sec"{print $5}')
[ "$(wc -w <<<"$key")" = 1 ] || { echo "expected exactly one secret key, found: $key" >&2; exit 1; }
gpg --batch --armor --export "$key" >"$OUT/frogger.asc"

# ---- APT ----
apt=$OUT/apt
install -d "$apt/pool/main"
cp "$IN"/*.deb "$apt/pool/main/"
archs=$(for d in "$apt"/pool/main/*.deb; do dpkg-deb -f "$d" Architecture; done | grep -vx all | sort -u)
[ -n "$archs" ] || archs="amd64 arm64"
(
  cd "$apt"
  for a in $archs; do
    install -d "dists/stable/main/binary-$a"
    # --arch: packages of this architecture plus "all" ones.
    apt-ftparchive --arch "$a" packages pool >"dists/stable/main/binary-$a/Packages"
    gzip -9kf "dists/stable/main/binary-$a/Packages"
  done
  apt-ftparchive \
    -o APT::FTPArchive::Release::Origin=FROGGER \
    -o APT::FTPArchive::Release::Label=FROGGER \
    -o APT::FTPArchive::Release::Suite=stable \
    -o APT::FTPArchive::Release::Codename=stable \
    -o APT::FTPArchive::Release::Components=main \
    -o "APT::FTPArchive::Release::Architectures=$(echo $archs)" \
    -o APT::FTPArchive::Release::Description="FROGGER face authentication" \
    release dists/stable >"$OUT/Release.tmp"
  mv "$OUT/Release.tmp" dists/stable/Release
  gpg --batch --yes --local-user "$key" --clearsign -o dists/stable/InRelease dists/stable/Release
  gpg --batch --yes --local-user "$key" --armor --detach-sign -o dists/stable/Release.gpg dists/stable/Release
)

# ---- DNF ----
rpm=$OUT/rpm
install -d "$rpm"
cp "$IN"/*.rpm "$rpm/"
createrepo_c --quiet "$rpm"
gpg --batch --yes --local-user "$key" --armor --detach-sign -o "$rpm/repodata/repomd.xml.asc" "$rpm/repodata/repomd.xml"
# The metadata is signed and lists the checksum of every package.
cat >"$OUT/frogger.repo" <<REPO
[frogger]
name=FROGGER face authentication
baseurl=$BASE/rpm
enabled=1
repo_gpgcheck=1
gpgcheck=0
gpgkey=$BASE/frogger.asc
REPO

# ---- landing page ----
cat >"$OUT/index.html" <<HTML
<!doctype html><meta charset="utf-8"><title>FROGGER packages</title>
<h1>FROGGER package repository</h1>
<p>Installation: see the <a href="https://github.com/carstenkiefer/frogger#installation">README</a>.</p>
<ul><li><a href="frogger.asc">Signing key</a> (fingerprint $(gpg --batch --with-colons --fingerprint "$key" | awk -F: '$1=="fpr"{print $10; exit}'))</li>
<li><a href="apt/">APT repository</a> · <a href="frogger.repo">DNF repository</a></li></ul>
HTML
echo "repository in $OUT (key $key)"
