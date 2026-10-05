#!/bin/sh
# Fail unless the selected release workflow run built this exact target.
set -eu

: "${GH_TOKEN:?GH_TOKEN is required}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${TARGET:?TARGET is required}"

api() {
    curl -fsSL --retry 3 \
        -H "Authorization: Bearer $GH_TOKEN" \
        -H 'Accept: application/vnd.github+json' \
        "https://api.github.com/repos/$GITHUB_REPOSITORY/$1"
}

if [ "${GITHUB_EVENT_NAME:-}" = workflow_run ] &&
    jq -e '.workflow_run.event == "push" and (.workflow_run.head_branch | type == "string" and startswith("v"))' "$GITHUB_EVENT_PATH" >/dev/null; then
    run_id=$(jq -r '.workflow_run.id // empty' "$GITHUB_EVENT_PATH")
else
    # A main-branch push bootstraps the badges for an already published tag.
    runs=$(api 'actions/workflows/release.yml/runs?event=push&per_page=100')
    run_id=$(printf '%s' "$runs" | jq -r \
        '[.workflow_runs[] | select(.head_branch | type == "string" and startswith("v"))][0].id // empty')
fi
[ -n "$run_id" ] || { echo 'No tagged release workflow run found' >&2; exit 1; }

jobs=$(api "actions/runs/$run_id/jobs?filter=latest&per_page=100")
result=$(printf '%s' "$jobs" | jq -r --arg target "$TARGET" \
    '[.jobs[] | select(.name | contains($target))] | if length == 1 then .[0].conclusion else "missing-or-duplicate" end')
printf 'Release run %s, target %s: %s\n' "$run_id" "$TARGET" "$result"
[ "$result" = success ]
