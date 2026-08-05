#!/usr/bin/env bash
set -euo pipefail

typeshed_checkout="${1:?usage: benchmark-typeshed.sh TYPEHED_CHECKOUT [MAX_SECONDS]}"
max_seconds="${2:-10}"
binary="${NO_DEFAULTS_BIN:-target/release/no-defaults}"
output="$(mktemp)"
trap 'rm -f "$output"' EXIT

start_ns="$(date +%s%N)"
set +e
"$binary" --output-format concise "$typeshed_checkout" >"$output"
exit_code=$?
set -e
end_ns="$(date +%s%N)"

if [ "$exit_code" -ne 1 ]; then
  echo "expected violations (exit 1), got exit $exit_code" >&2
  exit 1
fi

summary="$(tail -n 1 "$output")"
violations="$(awk '/^Found [0-9]+ errors?\.$/ {print $2}' <<<"$summary")"
if [ -z "$violations" ] || [ "$violations" -lt 10000 ]; then
  echo "unexpected Typeshed result: $summary" >&2
  exit 1
fi

elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))
max_ms=$(( max_seconds * 1000 ))
printf 'Typeshed: %s violations in %s ms\n' "$violations" "$elapsed_ms"
if [ "$elapsed_ms" -gt "$max_ms" ]; then
  echo "benchmark exceeded ${max_seconds}s budget" >&2
  exit 1
fi
