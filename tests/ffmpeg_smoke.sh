#!/usr/bin/env bash

set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
run_dir=$(mktemp -d /tmp/pathshim-ffmpeg-smoke.XXXXXX)
ffmpeg_image=${PATHSHIM_FFMPEG_IMAGE:?set PATHSHIM_FFMPEG_IMAGE to an image containing ffmpeg and ffprobe}
ffmpeg_bin=${PATHSHIM_FFMPEG_BIN:-/usr/bin/ffmpeg}
ffprobe_bin=${PATHSHIM_FFPROBE_BIN:-/usr/bin/ffprobe}

cleanup() {
  rm -rf -- "$run_dir"
}
trap cleanup EXIT

for command in cargo docker; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "missing required command: $command" >&2
    exit 1
  }
done
docker info >/dev/null
docker image inspect "$ffmpeg_image" >/dev/null

mkdir -p "$run_dir/input" "$run_dir/output"

RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --manifest-path "$repo_dir/Cargo.toml" --locked --release \
  --target-dir "$run_dir/target"
pathshim="$run_dir/target/release/pathshim"
caller="$(id -u):$(id -g)"

docker run --rm \
  --cap-drop=ALL \
  --security-opt no-new-privileges=true \
  --user "$caller" \
  --volume "$run_dir/input:/fixture" \
  --entrypoint "$ffmpeg_bin" \
  "$ffmpeg_image" \
  -hide_banner -loglevel error -y \
  -f lavfi -i sine=frequency=1000:duration=0.2 \
  -c:a pcm_s16le /fixture/input.wav

docker run --rm \
  --cap-drop=ALL \
  --security-opt no-new-privileges=true \
  --user "$caller" \
  --volume "$pathshim:/pathshim:ro" \
  --volume "$run_dir/input:/fixture:ro" \
  --volume "$run_dir/output:/physical-output" \
  --entrypoint /pathshim \
  "$ffmpeg_image" \
  --bind /physical-output:/output -- "$ffmpeg_bin" \
  -hide_banner -loglevel error -y \
  -i /fixture/input.wav -c:a pcm_s16le /output/transcoded.wav \
  2>"$run_dir/pathshim.stderr"

case "$(<"$run_dir/pathshim.stderr")" in
  *"collect mode=bind-view"*) ;;
  *)
    cat "$run_dir/pathshim.stderr" >&2
    exit 1
    ;;
esac
test -s "$run_dir/output/transcoded.wav"

duration=$(docker run --rm \
  --cap-drop=ALL \
  --security-opt no-new-privileges=true \
  --user "$caller" \
  --volume "$run_dir/output:/output:ro" \
  --entrypoint "$ffprobe_bin" \
  "$ffmpeg_image" \
  -v error -show_entries format=duration \
  -of default=noprint_wrappers=1:nokey=1 \
  /output/transcoded.wav)

test "$duration" = "0.200000"
printf 'pathshim ffmpeg smoke passed: arch=%s bytes=%s duration=%s\n' \
  "$(uname -m)" \
  "$(wc -c < "$run_dir/output/transcoded.wav" | tr -d ' ')" \
  "$duration"
