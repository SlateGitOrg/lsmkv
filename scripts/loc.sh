#!/usr/bin/env bash
# Counts lines of Rust: library/binary source (excluding in-file
# `#[cfg(test)]` modules and src/**/tests.rs), and test code.
set -euo pipefail
cd "$(dirname "$0")/.."
src=0; inline_tests=0; code=0
while IFS= read -r f; do
  total=$(wc -l < "$f")
  case "$f" in
    */tests.rs) inline_tests=$((inline_tests + total)); continue ;;
  esac
  cut=$(grep -n '^#\[cfg(test)\]' "$f" | head -1 | cut -d: -f1 || true)
  keep=$total
  if [ -n "$cut" ]; then
    keep=$((cut - 1)); inline_tests=$((inline_tests + total - cut + 1))
  fi
  src=$((src + keep))
  c=$(head -n "$keep" "$f" | grep -cvE '^[[:space:]]*(//.*)?$' || true)
  code=$((code + c))
done < <(find src examples -name '*.rs' | sort)
integ=$(cat tests/*.rs tests/common/*.rs | wc -l | tr -d ' ')
echo "non-test source (src/ + examples/): $src lines ($code excluding blank and comment lines)"
echo "unit tests inside src/:             $inline_tests lines"
echo "integration/property/crash tests:   $integ lines"
