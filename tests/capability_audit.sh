#!/usr/bin/env bash

set -euo pipefail

if [[ $(uname -s) != Linux ]]; then
  echo "capability audit requires Linux" >&2
  exit 1
fi

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
run_dir=$(mktemp -d /tmp/pathshim-capability-audit.XXXXXX)

cleanup() {
  rm -rf -- "$run_dir"
}
trap cleanup EXIT

for command in cargo ln; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "missing required command: $command" >&2
    exit 1
  }
done

cargo build --manifest-path "$repo_dir/Cargo.toml" --locked \
  --target-dir "$run_dir/target" >/dev/null
pathshim="$run_dir/target/debug/pathshim"
lower="$run_dir/lower"
upper="$run_dir/upper"
mkdir -p "$lower" "$upper"
printf 'hard-link-source\n' > "$lower/source.txt"

"$pathshim" --rootfs "$upper" -- "$(command -v ln)" \
  "$lower/source.txt" "$lower/result.txt" \
  2>"$run_dir/pathshim.stderr"

mapped_result="$upper/${lower#/}/result.txt"
if [[ -f $mapped_result && ! -e $lower/result.txt ]]; then
  coverage=projected
  result=$mapped_result
elif [[ -f $lower/result.txt && ! -e $mapped_result ]]; then
  coverage=lower-bypass
  result=$lower/result.txt
else
  echo "hard-link result is missing or exists in multiple views" >&2
  exit 1
fi
test "$(cat "$result")" = 'hard-link-source'

mode=$(sed -n 's/^pathshim: collect mode=\([^ ]*\).*/\1/p' "$run_dir/pathshim.stderr")
printf 'pathshim capability audit: arch=%s mode=%s hard-link=%s\n' \
  "$(uname -m)" "$mode" "$coverage"
