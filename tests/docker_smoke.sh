#!/usr/bin/env bash

set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
run_dir=$(mktemp -d /tmp/pathshim-docker-smoke.XXXXXX)
image="pathshim-docker-smoke:run-$$-$(uname -m)"

cleanup() {
  docker image rm -f "$image" >/dev/null 2>&1 || true
  rm -rf "$run_dir"
}
trap cleanup EXIT

for command in cargo docker go tar; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "missing required command: $command" >&2
    exit 1
  }
done
docker info >/dev/null

mkdir -p "$run_dir/image" "$run_dir/rootfs"

RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --manifest-path "$repo_dir/Cargo.toml" --locked --release \
  --target-dir "$run_dir/target"
CGO_ENABLED=0 go build -o "$run_dir/image/fixture" \
  "$repo_dir/tests/fixtures/static_go.go"
cp "$run_dir/target/release/pathshim" "$run_dir/image/pathshim"

tar -C "$run_dir/image" -cf "$run_dir/image.tar" .
docker import "$run_dir/image.tar" "$image" >/dev/null

output=$(docker run --rm \
  --cap-drop=ALL \
  --security-opt no-new-privileges=true \
  --user "$(id -u):$(id -g)" \
  --volume "$run_dir/rootfs:/rootfs" \
  "$image" \
  /pathshim --rootfs /rootfs -- /fixture)

test -s "$run_dir/rootfs/project/go-output"
test "$output" = "$(cat "$run_dir/rootfs/project/go-output")"

printf 'pathshim Docker smoke passed: arch=%s output=%s\n' "$(uname -m)" "$output"
