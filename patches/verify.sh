#!/usr/bin/env bash
# Checks each version's patch series against the revision it records, and — given a bazel
# checkout — that it still applies.
#
# Usage: verify.sh [--only VERSION] [path-to-bazelbuild/bazel]
#
# Without a checkout it verifies only what is in this directory, which is what CI can do without
# cloning Bazel. The repository argument is for a human rebasing a patch.
set -o errexit -o nounset -o pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

only=""
if [ "${1:-}" = "--only" ]; then
    only=${2:?--only needs a version}
    shift 2
fi
repo=${1:-}

verify_one() { # version-directory
    local dir=$1 version
    version=$(basename "$dir")

    # shellcheck source=/dev/null
    source "$dir/BAZEL_VERSION"

    # The directory name is what a reader keys on, so it has to agree with the file.
    if [ "$version" != "$BAZEL_RELEASE" ]; then
        echo "$version/: BAZEL_VERSION says BAZEL_RELEASE=$BAZEL_RELEASE" >&2
        return 1
    fi

    # A series, applied in name order. `git format-patch --base` writes the trailer on the first.
    local patches=("$dir"/*.patch)
    if [ ! -f "${patches[0]}" ]; then
        echo "$version/: no .patch files" >&2
        return 1
    fi

    local carried
    carried=$(sed -n 's/^base-commit: //p' "${patches[0]}")
    if [ -z "$carried" ]; then
        echo "$version/: no base-commit trailer; regenerate with --base=$BAZEL_RELEASE" >&2
        return 1
    fi
    if [ "$carried" != "$BAZEL_COMMIT" ]; then
        echo "$version/: BAZEL_VERSION records $BAZEL_COMMIT but the patch is based on $carried" >&2
        return 1
    fi
    echo "$version: base matches BAZEL_VERSION ($BAZEL_COMMIT)"

    [ -z "$repo" ] && return 0

    if ! git -C "$repo" cat-file -e "$BAZEL_COMMIT^{commit}" 2>/dev/null; then
        echo "$version/: $repo has no commit $BAZEL_COMMIT; fetch the $BAZEL_RELEASE tag" >&2
        return 1
    fi

    # A detached worktree at the recorded base, not the checkout's HEAD: the question is whether the
    # patch still describes that release, not whatever the working tree happens to be on.
    local tree
    tree=$(mktemp -d)
    rmdir "$tree"
    git -C "$repo" worktree add --detach --quiet "$tree" "$BAZEL_COMMIT"
    local rc=0
    # `am` rather than `apply --check`: each patch in a series applies on top of the one before.
    git -C "$tree" -c user.name=verify -c user.email=verify@invalid am --quiet "${patches[@]}" ||
        rc=1
    git -C "$repo" worktree remove --force "$tree" 2>/dev/null || true
    rm -rf "$tree"
    if [ "$rc" -ne 0 ]; then
        echo "$version/: the patch no longer applies to $BAZEL_RELEASE" >&2
        return 1
    fi
    echo "$version: applies cleanly to $BAZEL_RELEASE"
}

status=0
found=0
for dir in "$here"/*/; do
    [ -f "$dir/BAZEL_VERSION" ] || continue
    [ -n "$only" ] && [ "$(basename "$dir")" != "$only" ] && continue
    found=$((found + 1))
    verify_one "${dir%/}" || status=1
done

if [ "$found" -eq 0 ]; then
    echo "no version directories found${only:+ matching $only}" >&2
    exit 1
fi
exit "$status"
