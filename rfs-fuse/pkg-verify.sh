#!/usr/bin/env bash
# Build + verify the rfs-fuse Debian/RPM packages on Linux.
set -uo pipefail
SRC="/mnt/d/Paul/Software Projects/RFS"
WORK="$HOME/rfspkg"

rm -rf "$WORK"
mkdir -p "$WORK"
cp -a "$SRC/." "$WORK/"
rm -rf "$WORK/.git" "$WORK/target" "$WORK/rfs-fuse/target" "$WORK/fuzz/target"
cd "$WORK"

echo "== cargo deb =="
cargo deb --manifest-path rfs-fuse/Cargo.toml 2>&1 | tail -3
echo "== cargo generate-rpm =="
cargo build --release --manifest-path rfs-fuse/Cargo.toml >/dev/null 2>&1
cargo generate-rpm -p rfs-fuse 2>&1 | tail -3

DEB=$(ls "$WORK"/rfs-fuse/target/debian/*.deb 2>/dev/null | head -1)
RPM=$(ls "$WORK"/target/generate-rpm/*.rpm 2>/dev/null | head -1)
echo "== built: $(basename "$DEB")  |  $(basename "$RPM") =="

echo "== .deb metadata =="; dpkg-deb -I "$DEB" | sed -n '1,20p'
echo "== .deb contents =="; dpkg-deb -c "$DEB"

echo "== install .deb =="
sudo dpkg --purge --force-all rfs-fuse >/dev/null 2>&1 || true
sudo dpkg -i "$DEB" 2>&1 | tail -3        # deps (libfuse3-*, libc6) pre-satisfied by build deps
echo "installed binary: $(command -v rfs-fuse)"
echo "installed man:    $(dpkg -L rfs-fuse | grep man1 || echo '(none)')"

echo "== mount via installed binary =="
MNT=/tmp/pkgmnt; IMG=/tmp/pkg.img
fusermount3 -u "$MNT" 2>/dev/null || true
mkdir -p "$MNT"; rm -f "$IMG"
nohup rfs-fuse "$IMG" "$MNT" >/tmp/pkg.log 2>&1 &
for _ in $(seq 1 50); do grep -q "$MNT" /proc/mounts && break; sleep 0.1; done
mkdir "$MNT/d" && echo "packaged ok" > "$MNT/d/f.txt"
echo "read back: $(cat "$MNT/d/f.txt")"
df -h "$MNT" | sed -n 2p
fusermount3 -u "$MNT"

echo "== rpm summary =="
if command -v rpm >/dev/null; then rpm -qpi "$RPM" 2>/dev/null | sed -n '1,8p'; else echo "(rpm tool not installed; built $(basename "$RPM"))"; fi
echo "DONE"
