#!/usr/bin/env bash
# Build a demo superproject for manually testing taid.
#
#   subA — clean and current, with an extra `old` branch at its first commit
#   subB — drifted: the superproject records the newest commit, the working
#          directory is one behind
#   subC — recorded but never initialized (checkout removed)
#
# The main repo also gets an `experiment` branch to try the branch switcher.
#
# Usage: bash dev/fixtures.sh [dest]     (default: ./taid-demo)
# Requires: git. The repos are fully local — nothing touches the network.

set -euo pipefail

DEST="${1:-./taid-demo}"

if [ -e "$DEST" ]; then
    echo "refusing to overwrite existing path: $DEST" >&2
    exit 1
fi

mkdir -p "$DEST"
cd "$DEST"

commit() { # commit <file> <content> <message> <date>
    printf '%s\n' "$2" > "$1"
    git add .
    GIT_AUTHOR_DATE="$4" GIT_COMMITTER_DATE="$4" git commit -qm "$3"
}

echo "==> building subA (3 commits + an 'old' branch)"
git init -q subA
(
    cd subA
    commit a1.txt "subA content 1" "subA commit 1" "2024-01-01 10:00:00 +0100"
    commit a2.txt "subA content 2" "subA commit 2" "2024-01-02 10:00:00 +0100"
    commit a3.txt "subA content 3" "subA commit 3" "2024-01-03 10:00:00 +0100"
    git branch old HEAD~2
)

echo "==> building subB (2 commits)"
git init -q subB
(
    cd subB
    commit b1.txt "subB content 1" "subB commit 1" "2024-01-01 11:00:00 +0100"
    commit b2.txt "subB content 2" "subB commit 2" "2024-01-02 11:00:00 +0100"
)

echo "==> building subC (1 commit)"
git init -q subC
(
    cd subC
    commit c1.txt "subC content 1" "subC commit 1" "2024-01-01 12:00:00 +0100"
)

echo "==> building the superproject"
git init -q super
(
    cd super
    commit README.md "# super" "root" "2024-01-04 10:00:00 +0100"
    git -c protocol.file.allow=always submodule add -q ../subA subA
    git -c protocol.file.allow=always submodule add -q ../subB subB
    git -c protocol.file.allow=always submodule add -q ../subC subC
    # Commit the submodule additions without touching .gitmodules.
    git add .gitmodules
    GIT_AUTHOR_DATE="2024-01-04 10:01:00 +0100" \
        GIT_COMMITTER_DATE="2024-01-04 10:01:00 +0100" \
        git commit -qm "add submodules"

    # subB drifts: working directory one commit behind the recorded pointer.
    (cd subB && git checkout -q --detach HEAD~1)

    # subC stays uninitialized.
    rm -rf subC

    # A branch to play with the branch switcher.
    git branch experiment
)

echo
echo "Demo repository ready:"
echo "  cd $DEST/super"
echo "  taid"
echo
echo "Things to try:"
echo "  - browse subA/subB histories (hash, date and title of every commit)"
echo "  - pick an older commit for subA or subB, then s → Apply"
echo "  - after saving: git status shows the staged pointer change (M subA),"
echo "    and git log inside the submodule still lists the newer commits"
echo "  - b opens the branch switcher (try 'experiment' or subA's 'old')"
echo "  - subC shows the not-initialized state"
