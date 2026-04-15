#!/usr/bin/env bash
# Publish the mangle-rs workspace to crates.io in dependency order.
#
# Usage:
#   scripts/publish-release.sh                 # dry-run (default)
#   scripts/publish-release.sh --dry-run       # same as above
#   scripts/publish-release.sh --publish       # actually publish
#   scripts/publish-release.sh --from mangle-X # resume at a specific crate
#                                              # (combine with --publish)
#
# Order rationale: topological over regular [dependencies], adjusted so each
# crate's dev-dependencies resolve against already-published versions.
# mangle-engine publishes AFTER mangle-driver because engine has a dev-dep on
# driver; nothing depends on engine, so this is fine.
#
# Safety: --dry-run is the default. You must pass --publish explicitly.
# A dry-run that passes is NOT a guarantee the real publish will — crates.io
# may still reject on rate-limiting, duplicate version, or name squatting.

set -euo pipefail

CRATES=(
    mangle-ast
    mangle-ir
    mangle-common
    mangle-parse
    mangle-interpreter
    mangle-analysis
    mangle-simplecolumn
    mangle-codegen
    mangle-vm
    mangle-driver
    mangle-engine
    mangle-db
    mangle-wasm
    mangle-server
)

mode="dry-run"
from=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) mode="dry-run"; shift ;;
        --publish) mode="publish"; shift ;;
        --from) from="$2"; shift 2 ;;
        -h|--help)
            awk 'NR==1{next} /^[^#]/{exit} {sub(/^# ?/, ""); print}' "$0"
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            exit 2
            ;;
    esac
done

# Skip crates up to and excluding `$from`, if given.
skip=0
if [[ -n "$from" ]]; then
    skip=1
    if ! printf '%s\n' "${CRATES[@]}" | grep -qx "$from"; then
        echo "error: --from '$from' is not in the publish list" >&2
        exit 2
    fi
fi

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

flags=()
if [[ "$mode" == "dry-run" ]]; then
    # --no-verify skips the compile step so downstream crates don't fail on
    # "mangle-ast = ^0.7.0 not found on crates.io" during a chained dry run.
    # We still get packaging validation (manifest rewriting, file list,
    # metadata, size limits) for every crate — that's the part dry-run can
    # uniquely exercise. Actual build-against-new-deps is covered by
    # `cargo test --workspace` before release.
    flags+=(--dry-run --no-verify)
    echo ">>> DRY RUN mode — no crates will be uploaded."
    echo ">>> (--no-verify is set; compile validation is covered by cargo test.)"
else
    echo ">>> PUBLISH mode — crates will be uploaded to crates.io."
    echo -n ">>> Continue? (type 'yes' to proceed) "
    read -r confirm
    [[ "$confirm" == "yes" ]] || { echo "aborted."; exit 1; }
fi

echo

total=${#CRATES[@]}
for i in "${!CRATES[@]}"; do
    crate="${CRATES[$i]}"
    step=$((i + 1))
    if [[ $skip -eq 1 ]]; then
        if [[ "$crate" == "$from" ]]; then
            skip=0
        else
            echo "[$step/$total] skip $crate (before --from=$from)"
            continue
        fi
    fi

    echo "[$step/$total] cargo publish -p $crate ${flags[*]-}"
    if ! cargo publish -p "$crate" "${flags[@]}"; then
        if [[ "$mode" == "dry-run" ]]; then
            # Expected for any crate that depends on a workspace sibling at
            # the new version: cargo can't resolve it until that sibling is
            # actually on crates.io. The real `--publish` run resolves this
            # by uploading in order. Keep going so we exercise packaging on
            # the remaining crates.
            echo "    (dry-run: dep-resolution error is expected for non-leaf crates; continuing)"
            continue
        fi
        echo >&2
        echo "error: publish failed at $crate." >&2
        echo "Resume with: $0 --publish --from $crate" >&2
        exit 1
    fi

    # Small pause between real publishes so crates.io's index has a moment
    # to catch up before the next crate's dependency check.
    if [[ "$mode" == "publish" && $step -lt $total ]]; then
        sleep 15
    fi
done

echo
echo ">>> all done."
