#!/bin/bash
# Who does the work in each configuration?
#
# The discriminator between "composited" and "scanned out" is the compositor's own
# GPU time, not the plane count: a composited frame and a directly scanned out one
# both occupy exactly one plane.
#
#   gpuprobe.sh <compositor-comm> [8-bit clip] [10-bit clip]
#
# Test clips, if you have none (needs an ffmpeg with an HEVC encoder; the Pi has no
# HEVC encoder, so make them elsewhere):
#
#   ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=60:duration=60 \
#          -pix_fmt yuv420p -c:v libx265 -b:v 20M test1080p8.mkv
#   ffmpeg -f lavfi -i testsrc2=size=1920x1080:rate=60:duration=60 \
#          -pix_fmt p010le -c:v libx265 -profile:v main10 -b:v 25M test1080p10.mkv
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
COMP="${1:?usage: gpuprobe.sh <compositor-comm> [8-bit clip] [10-bit clip]}"
CLIP8="${2:-$HERE/test1080p8.mkv}"
CLIP10="${3:-$HERE/test1080p10.mkv}"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-$(cd "$XDG_RUNTIME_DIR" && ls -1 wayland-[0-9] 2>/dev/null | head -1)}"
[ -n "$WAYLAND_DISPLAY" ] || { echo "no wayland socket in $XDG_RUNTIME_DIR"; exit 1; }

gpu() { # gpu <comm> -> total GPU ns across that process's DRM clients
	sudo -n bash -c "for p in \$(pgrep -x $1); do cat /proc/\$p/fdinfo/* 2>/dev/null; done" |
		awk '/drm-engine/{s+=$2} END{print s+0}'
}

sample() { # sample <label>
	local c0 c1 o0 o1 h0 h1
	c0=$(gpu "$COMP"); o0=$(gpu mpv)
	h0=$(grim - 2>/dev/null | md5sum)
	sleep 5
	c1=$(gpu "$COMP"); o1=$(gpu mpv)
	h1=$(grim - 2>/dev/null | md5sum)
	printf '%-38s %s: %5d ms/s  mpv: %5d ms/s  live: %s\n' "$1" "$COMP" \
		$(( (c1 - c0) / 5000000 )) $(( (o1 - o0) / 5000000 )) \
		"$([ "$h0" != "$h1" ] && echo yes || echo NO)"
	sudo -n "$HERE/planes.py" --comp "$COMP" 2>/dev/null | grep 'plane\[' | sed 's/^/      /'
}

start_mpv() { # start_mpv <vo> <clip>
	rm -f /tmp/mpvsock
	mpv --really-quiet --loop-file=inf --fullscreen --vo="$1" --hwdec=drm --no-audio \
		--input-ipc-server=/tmp/mpvsock "$2" >/dev/null 2>&1 &
	MPV=$!
	sleep 7
}

stop_all() { kill "${OVL:-0}" "${MPV:-0}" 2>/dev/null; sleep 2; OVL=0; MPV=0; }

echo "### $COMP on $WAYLAND_DISPLAY"
sample "idle"

if [ -f "$CLIP8" ]; then
	start_mpv dmabuf-wayland "$CLIP8"
	sample "8-bit video alone"
	"$HERE/overlay" -a -o 0.35 2>/dev/null & OVL=$!; sleep 4
	sample "  + translucent fullscreen UI"
	stop_all
else
	echo "(no 8-bit clip at $CLIP8, skipped)"
fi

if [ -f "$CLIP10" ]; then
	echo "### 10-bit: does the compositor even accept P030?"
	timeout 10 mpv --loop-file=inf --fullscreen --vo=dmabuf-wayland --hwdec=drm \
		--no-audio --msg-level=all=error,vo=v "$CLIP10" 2>&1 |
		grep -iE "not supported|Could not|reconfig" | head -3
else
	echo "(no 10-bit clip at $CLIP10, skipped)"
fi

"$HERE/overlay" -a -o 1.0 2>/dev/null & OVL=$!; sleep 4
sample "opaque fullscreen UI, no video"
stop_all
echo "### done"
