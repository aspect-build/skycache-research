#!/usr/bin/env bash
# Measures what a worktree switch costs when worktrees share one output base, with and without the
# analysis cache. See ../docs/experiments/shared-output-base.md.
#
#   A=/path/to/worktree-a B=/path/to/worktree-b TARGET=//app/... ACTIVE=app \
#     ./switch-cost.sh                                                      # analysis cache off
#   A=... B=... TARGET=... ACTIVE=... SKYCACHE=grpc://$(minikube ip):30051 \
#     HELPER=./local-credential-helper.sh ./switch-cost.sh                  # and on
#
# `--nobuild` stops after loading+analysis, which is the quantity a shared output base destroys
# and the analysis cache restores. Action execution is deliberately excluded: action results are
# keyed by action, not by workspace, so they are a constant across these arms and would otherwise
# dominate the wall time. It still uploads, so the upload leg is cheap too.
set -o nounset -o pipefail

A=${A:?set A to a worktree path}
B=${B:?set B to a second worktree path}
SP=${SP:-$(mktemp -d)}
BZL=${BZL:-bazel}
TARGET=${TARGET:?set TARGET to the pattern to analyze, e.g. //app/...}
# The directories being "worked on". The frontier is the boundary around them, and what gets
# serialized is everything *outside* it -- so naming the whole workspace would leave nothing to
# upload. Keep this to the target's own tree.
ACTIVE=${ACTIVE:?set ACTIVE to the directory TARGET lives in, e.g. app}
# An optional remote action cache, as `grpcs://host`, and the credential helper for it. Action
# results are a constant across the arms.
REMOTE=${REMOTE:-}
REMOTE_HELPER=${REMOTE_HELPER:-}
# Set both to measure with the analysis cache; leave unset for the control.
SKYCACHE=${SKYCACHE:-}
HELPER=${HELPER:-}

# A rust clippy aspect injected by the workspace bazelrc leaves a `.clippy.ok` marker the analysis
# cache serves back with the uploading machine's ctime/mtime/inode, and nothing can materialise a
# zero-byte marker in a fresh output base. Excluding the group is harmless where it does not exist.
common=(--nobuild --output_groups=-clippy_checks)
if [ -n "$REMOTE" ]; then
    remote_host=${REMOTE#*://}
    common+=(--remote_cache="$REMOTE")
    [ -n "$REMOTE_HELPER" ] && common+=(--credential_helper="${remote_host%%[:/]*}=$REMOTE_HELPER")
fi

analysis_flags() { # mode
    [ -z "$SKYCACHE" ] && return 0
    local host=${SKYCACHE#*://}
    host=${host%%:*}
    printf '%s\n' \
        "--experimental_remote_analysis_cache_mode=$1" \
        "--experimental_remote_analysis_cache=$SKYCACHE" \
        "--credential_helper=$host=$HELPER"
}

run() { # label workspace output_base mode [extra...]
    local label=$1 ws=$2 ob=$3 mode=$4
    shift 4
    local log
    log=$(mktemp)
    local start end
    cd "$ws" || return 1
    mapfile -t cache_flags < <(analysis_flags "$mode")
    start=$(date +%s.%N)
    "$BZL" --output_base="$ob" build "${common[@]}" \
        ${cache_flags+"${cache_flags[@]}"} "$@" "$TARGET" >"$log" 2>&1
    local rc=$?
    end=$(date +%s.%N)
    printf "%-36s wall=%6.1fs rc=%d  %s\n" "$label" "$(echo "$end - $start" | bc)" "$rc" \
        "$(grep -oE '[0-9,]+ targets configured' "$log" | tail -1)"
    grep -oE 'Skycache stats:.*\(([0-9.]+)%\)' "$log" | head -1 | sed 's/^/        /'
    [ "$rc" -ne 0 ] && grep -E "^(ERROR|FATAL)" "$log" | head -2 | sed 's/^/        /'
    rm -f "$log"
}

# A fresh output base per leg. Cleaning once up front instead measures a warm second leg and
# reports a speed-up that is mostly the first leg's leftovers.
wipe() {
    "$BZL" --output_base="$1" shutdown >/dev/null 2>&1
    chmod -R u+w "$1" 2>/dev/null
    rm -rf "$1"
}

OB=$SP/ob-switch
echo "### target=$TARGET analysis-cache=${SKYCACHE:-off}"
wipe "$OB"

if [ -n "$SKYCACHE" ]; then
    # Uploads need a cold Skyframe: BuildView.UPLOAD_BUILDS_MUST_BE_COLD is a hard error otherwise.
    run "A populate (UPLOAD)" "$A" "$OB" UPLOAD --experimental_active_directories="$ACTIVE"
    wipe "$OB"
fi

run "A cold" "$A" "$OB" DOWNLOAD
run "B after switch from A" "$B" "$OB" DOWNLOAD
run "A after switch back" "$A" "$OB" DOWNLOAD
run "A again, no switch" "$A" "$OB" DOWNLOAD
wipe "$OB"
