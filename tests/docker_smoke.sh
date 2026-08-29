#!/usr/bin/env bash

set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
run_dir=$(mktemp -d /tmp/pathshim-docker-smoke.XXXXXX)
image="pathshim-docker-smoke:run-$$-$(uname -m)"
deny_profile="$repo_dir/tests/fixtures/deny-seccomp.json"

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

mkdir -p \
  "$run_dir/image" \
  "$run_dir/rootfs" \
  "$run_dir/cwd-rootfs" \
  "$run_dir/bind-upper" \
  "$run_dir/bind-lower"

RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --manifest-path "$repo_dir/Cargo.toml" --locked --release \
  --target-dir "$run_dir/target"
CGO_ENABLED=0 go build -o "$run_dir/image/fixture" \
  "$repo_dir/tests/fixtures/static_go.go"
cp "$run_dir/target/release/pathshim" "$run_dir/image/pathshim"

tar -C "$run_dir/image" -cf "$run_dir/image.tar" .
docker import "$run_dir/image.tar" "$image" >/dev/null

docker_run=(docker run --rm \
  --cap-drop=ALL \
  --security-opt no-new-privileges=true \
  --user "$(id -u):$(id -g)")

assert_mode() {
  local log=$1
  local expected=$2
  case "$(<"$log")" in
    *"collect mode=$expected"*) ;;
    *)
      echo "expected collect mode $expected, got:" >&2
      cat "$log" >&2
      return 1
      ;;
  esac
}

cow_output=$("${docker_run[@]}" \
  --volume "$run_dir/rootfs:/rootfs" \
  "$image" \
  /pathshim --rootfs /rootfs -- /fixture \
  2>"$run_dir/cow.stderr")

test -s "$run_dir/rootfs/project/go-output"
test "$cow_output" = "$(cat "$run_dir/rootfs/project/go-output")"
assert_mode "$run_dir/cow.stderr" cow-view

cwd_output=$("${docker_run[@]}" \
  --security-opt "seccomp=$deny_profile" \
  --volume "$run_dir/cwd-rootfs:/rootfs" \
  "$image" \
  /pathshim --rootfs /rootfs -- /fixture relative-output \
  2>"$run_dir/cwd.stderr")

test -s "$run_dir/cwd-rootfs/relative-output"
test "$cwd_output" = "$(cat "$run_dir/cwd-rootfs/relative-output")"
assert_mode "$run_dir/cwd.stderr" cwd

passthrough_output=$("${docker_run[@]}" \
  --security-opt "seccomp=$deny_profile" \
  --volume "$run_dir/bind-upper:/upper" \
  --volume "$run_dir/bind-lower:/guest" \
  "$image" \
  /pathshim --bind /upper:/guest -- /fixture /guest/passthrough-output \
  2>"$run_dir/passthrough.stderr")

test -s "$run_dir/bind-lower/passthrough-output"
test ! -e "$run_dir/bind-upper/passthrough-output"
test "$passthrough_output" = "$(cat "$run_dir/bind-lower/passthrough-output")"
assert_mode "$run_dir/passthrough.stderr" passthrough

printf 'pathshim Docker smoke passed: arch=%s modes=cow-view,cwd,passthrough\n' "$(uname -m)"
