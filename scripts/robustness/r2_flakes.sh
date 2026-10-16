# R2 flakes: runs the whole test suite N times (default 10) inside the lab and
# lists the tests that failed, by count. Each run's output is kept in $OUT
# (default: $L/flakes). Run it on a quiet machine: the GPU tests
# (recorder_gpu_tests, vesper-encode) and the timing tests are load-sensitive.
# usage: TMO=7200 scripts/robustness/r2 flakes r2_flakes.sh N=10
# (r2 passes CARGO_HOME and RUSTUP_HOME in: the lab has its own HOME.)
. "${LABROOT:?not in the lab, use go or r2}/lib.sh" || exit 1
cd "$R" || exit 1
OUT=${OUT:-$L/flakes}
mkdir -p "$OUT"
echo "output in $OUT"
for i in $(seq 1 "${N:-10}"); do
  s=$(date +%s)
  cargo test --offline --workspace --no-fail-fast > "$OUT/run$i.txt" 2>&1
  rc=$?
  echo "run $i: rc=$rc $(($(date +%s) - s)) s: $(grep -E '^test result' "$OUT/run$i.txt" | awk '{p+=$4; f+=$6; i+=$8} END {print p " passed, " f " failed, " i " ignored"}')"
done
echo "--- failures by test"
grep -hE '^test .* FAILED$' "$OUT"/run*.txt | sort | uniq -c | sort -rn
