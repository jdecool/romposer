#!/usr/bin/env bash
# Cold/warm `install` of Symfony demo: composer, composer + pcomposer, romposer without
# and with the store. Prints the median wall time of N runs per cell.
#
#   bench/install.sh [N]   (default 3)
#
# Env: COMPOSER (default `composer`), ROMPOSER (default target/release/romposer),
#      PCOMPOSER_SRC (path to a pcomposer checkout; its column prints n/a when unset).
# Cold = empty cache dir (so empty zip cache and store), warm = cache dir from the
# previous run; vendor/ is deleted before every run. Every run uses --no-scripts, and the
# demo's own plugins (flex, runtime) are disabled because romposer never runs them.
# ponytail: plain wall-clock loop with no warmup or outlier rejection; use hyperfine if
# the numbers ever need confidence intervals.
set -euo pipefail
export LC_ALL=C # awk reads "1.5" as 1 under a decimal-comma locale

N=${1:-3}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
COMPOSER=${COMPOSER:-composer}
ROMPOSER=${ROMPOSER:-$ROOT/target/release/romposer}
DEMO_COMMIT=f42980c690b4bf335fa41bbafdb075bb67b719c2
WORK=${BENCH_DIR:-$ROOT/target/bench}

now() { perl -MTime::HiRes=time -e 'printf "%.3f\n", time'; }

mkdir -p "$WORK"
if [ ! -d "$WORK/demo/.git" ]; then
    git clone -q https://github.com/symfony/demo "$WORK/demo"
fi
git -C "$WORK/demo" checkout -q -f "$DEMO_COMMIT"
# Leaves the lock valid: allow-plugins is not part of content-hash.
COMPOSER_HOME="$WORK/home-setup" "$COMPOSER" -d "$WORK/demo" -q config allow-plugins.symfony/flex false
COMPOSER_HOME="$WORK/home-setup" "$COMPOSER" -d "$WORK/demo" -q config allow-plugins.symfony/runtime false

if [ -n "${PCOMPOSER_SRC:-}" ] && [ ! -d "$WORK/home-composer-pcomposer/vendor/jdecool/pcomposer" ]; then
    export COMPOSER_HOME="$WORK/home-composer-pcomposer"
    "$COMPOSER" global -q config repositories.pcomposer path "$PCOMPOSER_SRC"
    "$COMPOSER" global -q config allow-plugins.jdecool/pcomposer true
    "$COMPOSER" global -q require --no-interaction jdecool/pcomposer:@dev
    unset COMPOSER_HOME
fi

# run <tool> <mode>: prints one wall time in seconds.
run() {
    local tool=$1 mode=$2 cmd=$COMPOSER env=(PCOMPOSER_DISABLE=1)
    case $tool in
        composer-pcomposer) env=() ;;
        romposer) cmd=$ROMPOSER ;;
        romposer-store) cmd=$ROMPOSER env=() ;;
    esac
    [ "$mode" = cold ] && rm -rf "$WORK/cache-$tool"
    rm -rf "$WORK/demo/vendor"
    local start end
    start=$(now)
    env ${env[@]+"${env[@]}"} COMPOSER_HOME="$WORK/home-$tool" COMPOSER_CACHE_DIR="$WORK/cache-$tool" \
        "$cmd" -d "$WORK/demo" install --no-interaction --no-progress --no-scripts -q
    end=$(now)
    awk "BEGIN { print $end - $start }"
}

median() { sort -n | awk '{ v[NR] = $1 } END { printf "%.2f s", v[int((NR + 1) / 2)] }'; }

printf '| install (median of %s) | composer | composer + pcomposer | romposer, store disabled | romposer, store enabled |\n|---|---|---|---|---|\n' "$N"
for mode in cold warm; do
    row="| $mode |"
    for tool in composer composer-pcomposer romposer romposer-store; do
        if [ "$tool" = composer-pcomposer ] && [ -z "${PCOMPOSER_SRC:-}" ]; then
            row="$row n/a |"
            continue
        fi
        [ "$mode" = warm ] && run "$tool" warm >/dev/null # fill the cache once
        cell=$(for _ in $(seq "$N"); do run "$tool" "$mode"; done | median)
        row="$row $cell |"
    done
    echo "$row"
done
