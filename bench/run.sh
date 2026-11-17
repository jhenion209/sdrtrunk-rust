#!/usr/bin/env bash
# One benchmark run: start PROGRAM alone in a fresh settings folder, wait for warm-up (and its first MP3), sample it
# for MEASURE_SECONDS with measure.py, stop it, and write bench/results/ROUND-PROGRAM.{csv,meta}.
#
#   bench/run.sh [--dry-run] {sdrtrunk|sdrtrunk-vce|sdrtrunk-rust} ROUND
#
# Needs the templates made once in setup (see bench/README.md): ~/bench/template/sdrtrunk/SDRTrunk,
# ~/bench/template/vce-data, ~/bench/template/rust.db. Refuses to run next to the sdrtrunk-rust service or another
# of the three programs.
set -euo pipefail

DRY=0
if [[ ${1:-} == --dry-run ]]; then DRY=1; shift; fi
PROGRAM=${1:?program}; ROUND=${2:?round}
HERE=$(cd "$(dirname "$0")" && pwd)
BENCH=${BENCH:-$HOME/bench}
WARMUP=${WARMUP_SECONDS:-60}
MEASURE=${MEASURE_SECONDS:-600}
SDRTRUNK=${SDRTRUNK:-$HOME/sdrtrunk-1.0.0/sdr-trunk-linux-aarch64-v1.0.0}
VCE=${VCE:-$BENCH/vce}
RUST=${RUST:-$HERE/../target/release/sdrtrunk-rust}
RUN=$BENCH/run/$PROGRAM
# REPLAY_DIR: play the baseband recordings there instead of the SDRs (same signal for every program), with the
# templates made for it, a fixed warm-up and separate results.
REPLAY=${REPLAY_DIR:-}
if [[ -n $REPLAY ]]; then TEMPLATE=$BENCH/template-replay; RESULTS=$HERE/results-replay; else TEMPLATE=$BENCH/template; RESULTS=$HERE/results; fi
OUT=$RESULTS/$ROUND-$PROGRAM

case $PROGRAM in
  sdrtrunk)
    REC=$RUN/SDRTrunk/recordings
    VERSION=$(basename "$SDRTRUNK")
    PREPARE=(cp -a "$TEMPLATE/sdrtrunk/." "$RUN/")  # SDRTrunk/ and Java preferences (.java/)
    # Java takes user.home from the password file, not $HOME.
    LAUNCH=(env JAVA_OPTS="-Duser.home=$RUN" DISPLAY=:0 _JAVA_AWT_WM_NONREPARENTING=1 XAUTHORITY="$HOME/.Xauthority" "$SDRTRUNK/bin/sdr-trunk") ;;
  sdrtrunk-vce)
    REC=$VCE/data
    VERSION="sdrtrunk-vce nightly, java $(sed -n 's/^JAVA_VERSION="\(.*\)"/\1/p' "$VCE/release" 2>/dev/null || true)"
    PREPARE=(cp -a "$TEMPLATE/vce-data" "$VCE/data")
    LAUNCH=(env DISPLAY=:0 _JAVA_AWT_WM_NONREPARENTING=1 XAUTHORITY="$HOME/.Xauthority" "$VCE/Start VCE.sh") ;;
  sdrtrunk-rust)
    REC=$RUN/recordings
    VERSION="sdrtrunk-rust ${RUST_VERSION:-unknown}"
    PREPARE=(cp "$BENCH/template/rust.db" "$RUN/bench.db")
    LAUNCH=("$RUST" serve --db "$RUN/bench.db" --listen 127.0.0.1:8099 ${REPLAY:+--replay "$REPLAY"}) ;;
  *) echo "unknown program: $PROGRAM" >&2; exit 2 ;;
esac

if (( DRY )); then
  echo "settings folder: $RUN (fresh)"
  echo "prepare: ${PREPARE[*]}"
  echo "launch:  ${LAUNCH[*]}"
  echo "recordings: $REC"
  if [[ -n $REPLAY ]]; then echo "replay: $REPLAY (template $TEMPLATE, warm-up from the first MP3)"; fi
  echo "warm-up ${WARMUP}s, measure ${MEASURE}s -> $OUT.csv, $OUT.meta"
  echo "version: $VERSION"
  exit 0
fi

if systemctl is-active --quiet sdrtrunk-rust; then echo "stop the sdrtrunk-rust service first" >&2; exit 1; fi
if pgrep -x 'java|sdrtrunk-rust' >/dev/null; then echo "another program is running" >&2; exit 1; fi

rm -rf "$RUN"; mkdir -p "$RUN" "$RESULTS"
if [[ $PROGRAM == sdrtrunk-vce ]]; then rm -rf "$VCE/data"; fi
"${PREPARE[@]}"
# sdrtrunk-vce plays only regular files in data/recording_tuners (no symlinks): hard-link them, no 6 GB copy.
if [[ $PROGRAM == sdrtrunk-vce && -n $REPLAY ]]; then mkdir -p "$VCE/data/recording_tuners"; ln -f "$REPLAY"/*.wav "$VCE/data/recording_tuners/"; fi
if [[ $PROGRAM == sdrtrunk-rust ]]; then
  # The copy records into this run's folder, follows only listed talkgroups set to record, and streams nothing.
  python3 - "$RUN/bench.db" "$REC" <<'EOF'
import json, sqlite3, sys
c = sqlite3.connect(sys.argv[1])
s = json.loads((c.execute("SELECT json FROM settings WHERE id = 1").fetchone() or ["{}"])[0])
s["recording_dir"] = sys.argv[2]
c.execute("INSERT OR REPLACE INTO settings (id, json) VALUES (1, ?)", (json.dumps(s),))
c.execute("UPDATE unlisted SET record = 0")
c.execute("INSERT OR IGNORE INTO unlisted (system, record) SELECT name, 0 FROM alias_lists")
for i, j in c.execute("SELECT id, json FROM streams").fetchall():
    d = json.loads(j); d["enabled"] = False
    c.execute("UPDATE streams SET json = ? WHERE id = ?", (json.dumps(d), i))
c.commit()
EOF
  if [[ -n $REPLAY ]]; then python3 "$HERE/replay_db.py" --db "$RUN/bench.db" --replay "$REPLAY"; fi
fi
mkdir -p "$REC"

MARK=$RUN/.launched; touch "$MARK"
STARTED=$(date -Iseconds); T0=$(date +%s)
setsid "${LAUNCH[@]}" >"$RUN/console.log" 2>&1 &
PID=$!
if [[ -n $REPLAY && $PROGRAM != sdrtrunk-rust ]]; then echo "now enable both Recording tuners and start both channels in $PROGRAM"; fi
stop() {
  kill -TERM -- "-$PID" 2>/dev/null || true
  for _ in $(seq 15); do kill -0 "$PID" 2>/dev/null || return 0; sleep 1; done
  kill -KILL -- "-$PID" 2>/dev/null || true
}
trap stop EXIT

FIRST=""
while :; do
  sleep 1
  kill -0 "$PID" 2>/dev/null || { echo "program exited during warm-up (see $RUN/console.log)" >&2; break; }
  ELAPSED=$(( $(date +%s) - T0 ))
  if [[ -z $FIRST ]] && [[ -n $(find "$REC" -name '*.mp3' -newer "$MARK" -print -quit 2>/dev/null) ]]; then FIRST=$ELAPSED; fi
  if [[ -n $REPLAY ]]; then
    # Replay: measure from WARMUP s after the first MP3, the same for all three; the Java programs play only once
    # someone enables their recording tuners by hand (both start them disabled).
    { [[ -n $FIRST ]] && (( ELAPSED >= FIRST + WARMUP )); } || (( ELAPSED >= WARMUP + MEASURE )) && break
  else
    (( ELAPSED >= WARMUP )) && { [[ -n $FIRST ]] || (( ELAPSED >= WARMUP + MEASURE )); } && break
  fi
done

python3 "$HERE/measure.py" --pid "$PID" --seconds "$MEASURE" --recordings "$REC" --out "$OUT.csv" || true
EXIT=ok; kill -0 "$PID" 2>/dev/null || EXIT=exited-early
stop; trap - EXIT
printf 'program=%s\nversion=%s\nstarted=%s\nfirst_mp3_s=%s\nexit=%s\n' "$PROGRAM" "$VERSION" "$STARTED" "$FIRST" "$EXIT" > "$OUT.meta"
echo "done: $OUT.csv ($(($(wc -l < "$OUT.csv") - 1)) samples), first MP3 after ${FIRST:-none} s"
