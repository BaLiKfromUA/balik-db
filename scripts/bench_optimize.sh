#!/usr/bin/env bash
#
# Benchmark the impact of logical optimization on a SELECT with WHERE + ORDER BY
# + LIMIT. Generates a wide-row dataset once, then times the same query with and
# without `--optimize`.
#
# Usage:
#   scripts/bench_optimize.sh [SIZE] [DB_DIR]
#
#   SIZE    target dataset size, e.g. 1GB (default), 500MB, 2GiB.
#   DB_DIR  where to put the database (default: a fresh temp dir, removed on exit).
#
# Notes:
#   * The query column-pushes past three large TEXT payloads the SELECT never
#     reads and fuses ORDER BY + LIMIT into a TopK, so `--optimize` should both
#     decode far fewer bytes and skip a full sort.
#   * CLI timings include process startup + opening the store. That overhead is
#     small next to scanning the dataset, but it is not zero — for a noise-free
#     engine-only measurement you would time in-process instead.

set -euo pipefail

SIZE="${1:-1GB}"
DB_DIR="${2:-}"
SEED=0

# The benchmark query: two WHERE clauses, an ORDER BY, and a LIMIT. The filter
# midpoints (500000 against a 0..1,000,000 range) keep ~25% of rows.
QUERY="SELECT id, sort_key FROM bench \
WHERE filter_a > 500000 AND filter_b < 500000 \
ORDER BY sort_key LIMIT 20"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Report the host so saved results carry the machine they were measured on.
print_machine_info() {
  echo "Machine:"
  echo "  os:     $(uname -srm)"
  if [[ "$(uname -s)" == "Darwin" ]]; then
    echo "  cpu:    $(sysctl -n machdep.cpu.brand_string 2>/dev/null) ($(sysctl -n hw.ncpu 2>/dev/null) cores)"
    echo "  memory: $(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1024 / 1024 )) MB"
  else
    echo "  cpu:    $(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//') ($(nproc 2>/dev/null) cores)"
    echo "  memory: $(grep -m1 MemTotal /proc/meminfo 2>/dev/null | awk '{print $2/1024 " MB"}')"
  fi
  echo
}
print_machine_info

CLEANUP_PATH=""
if [[ -z "$DB_DIR" ]]; then
  # bench-gen initializes a fresh database, so point it at a not-yet-existing
  # subdir of a temp parent and clean up the whole parent on exit.
  CLEANUP_PATH="$(mktemp -d)"
  DB_DIR="$CLEANUP_PATH/db"
fi
cleanup() { [[ -n "$CLEANUP_PATH" ]] && rm -rf "$CLEANUP_PATH"; }
trap cleanup EXIT

echo "Building release binary..."
cargo build --release --quiet
BIN="$REPO_ROOT/target/release/balik-cli"

echo "Generating ~$SIZE of data into '$DB_DIR' (one-time)..."
"$BIN" bench-gen --db "$DB_DIR" --size "$SIZE" --seed "$SEED"
echo

echo "Generated table:"
"$BIN" query --db "$DB_DIR" --sql "DESCRIBE EXTENDED bench"
echo

echo "Query:"
echo "$QUERY"
echo

echo "Plan WITHOUT optimization:"
"$BIN" explain --db "$DB_DIR" --sql "$QUERY"
echo
echo "Plan WITH optimization:"
"$BIN" explain --db "$DB_DIR" --optimize --sql "$QUERY"
echo

echo "Timing (lower is better):"
if command -v hyperfine >/dev/null 2>&1; then
  hyperfine --warmup 1 --runs 10 \
    --command-name "no-optimize" \
    "$BIN query --db $DB_DIR --sql \"$QUERY\"" \
    --command-name "optimize" \
    "$BIN query --db $DB_DIR --optimize --sql \"$QUERY\""
else
  echo "(hyperfine not found; falling back to a timed loop)"
  time_variant() {
    local label="$1"; shift
    local runs=5 total=0 t
    # Warm the page cache once before measuring.
    "$@" >/dev/null
    for _ in $(seq "$runs"); do
      local s e
      s=$(date +%s.%N)
      "$@" >/dev/null
      e=$(date +%s.%N)
      t=$(echo "$e - $s" | bc)
      total=$(echo "$total + $t" | bc)
    done
    printf "  %-12s avg %.3fs over %d runs\n" "$label" "$(echo "$total / $runs" | bc -l)" "$runs"
  }
  time_variant "no-optimize" "$BIN" query --db "$DB_DIR" --sql "$QUERY"
  time_variant "optimize"    "$BIN" query --db "$DB_DIR" --optimize --sql "$QUERY"
fi
