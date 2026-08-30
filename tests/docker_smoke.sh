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
  "$run_dir/bind-source" \
  "$run_dir/bind-destination" \
  "$run_dir/passthrough-source" \
  "$run_dir/passthrough-destination"

RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --manifest-path "$repo_dir/Cargo.toml" --locked --release \
  --target-dir "$run_dir/target"
CGO_ENABLED=0 go build -o "$run_dir/image/fixture" \
  "$repo_dir/tests/fixtures/static_go.go"
cp "$run_dir/target/release/pathshim" "$run_dir/image/pathshim"
cp "$run_dir/image/fixture" "$run_dir/bind-source/app"

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

assert_probe_mode() {
  local log=$1
  local expected=$2
  case "$(<"$log")" in
    *"probe mode=$expected"*) ;;
    *)
      echo "expected probe mode $expected, got:" >&2
      cat "$log" >&2
      return 1
      ;;
  esac
}

probe_output=$("${docker_run[@]}" \
  --volume "$run_dir/bind-source:/source" \
  --volume "$run_dir/bind-destination:/guest" \
  "$image" \
  /pathshim probe --bind /source:/guest \
  2>"$run_dir/probe-bind.stderr")
test "$probe_output" = "bind-view"
test ! -s "$run_dir/probe-bind.stderr"

set +e
probe_passthrough_output=$("${docker_run[@]}" \
  --security-opt "seccomp=$deny_profile" \
  --volume "$run_dir/passthrough-source:/source" \
  --volume "$run_dir/passthrough-destination:/guest" \
  "$image" \
  /pathshim probe --bind /source:/guest \
  2>"$run_dir/probe-passthrough.stderr")
probe_passthrough_status=$?
set -e
test "$probe_passthrough_status" -eq 1
test "$probe_passthrough_output" = "passthrough"
assert_probe_mode "$run_dir/probe-passthrough.stderr" passthrough

bind_output=$("${docker_run[@]}" \
  --volume "$run_dir/bind-source:/source" \
  --volume "$run_dir/bind-destination:/guest" \
  "$image" \
  /pathshim --bind /source:/guest -- /guest/app /guest/go-output \
  2>"$run_dir/bind.stderr")

test -s "$run_dir/bind-source/go-output"
test ! -e "$run_dir/bind-destination/go-output"
test "$bind_output" = "$(cat "$run_dir/bind-source/go-output")"
assert_mode "$run_dir/bind.stderr" bind-view

passthrough_output=$("${docker_run[@]}" \
  --security-opt "seccomp=$deny_profile" \
  --volume "$run_dir/passthrough-source:/source" \
  --volume "$run_dir/passthrough-destination:/guest" \
  "$image" \
  /pathshim --bind /source:/guest -- /fixture /guest/passthrough-output \
  2>"$run_dir/passthrough.stderr")

test -s "$run_dir/passthrough-destination/passthrough-output"
test ! -e "$run_dir/passthrough-source/passthrough-output"
test "$passthrough_output" = "$(cat "$run_dir/passthrough-destination/passthrough-output")"
assert_mode "$run_dir/passthrough.stderr" passthrough

printf 'pathshim Docker smoke passed: arch=%s modes=bind-view,passthrough probe=verified\n' "$(uname -m)"
