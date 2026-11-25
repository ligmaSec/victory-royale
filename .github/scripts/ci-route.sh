#!/usr/bin/env bash
# Report which jobs the checked-out event tree requires.
set -euo pipefail
tree=$(git rev-parse 'HEAD^{tree}')
route=code
case "$GITHUB_EVENT_NAME" in
    pull_request) base=$(git rev-parse 'HEAD^1') ;;
    push) base=${BEFORE_SHA:-} ;;
    *) base='' ;;
esac

docs_only() {
    # Test both sides of renames and every commit in the push or merged PR.
    local base_commit=$1
    git diff --quiet --no-renames "$base_commit" HEAD -- . \
        ':(top,exclude)README.md' ':(top,exclude)CONTRIBUTING.md' ':(top,exclude)AGENTS.md' \
        ':(top,exclude,glob)docs/**/*.md' ':(top,exclude)tests/runtime/README.md' \
        ':(top,exclude)tests/metal/README.md'
}

already_tested() {
    local repo="repos/$GITHUB_REPOSITORY" pr_info number head run job
    # Multi-commit direct pushes can contain earlier changes that missed PR CI.
    [[ $base == "$(git rev-parse 'HEAD^1')" ]] || return 1
    pr_info=$(gh api "$repo/commits/$GITHUB_SHA/pulls" --jq \
        ".[] | select(.merged_at and .base.ref == \"main\" and .base.repo.full_name == \"$GITHUB_REPOSITORY\" and .merge_commit_sha == \"$GITHUB_SHA\") | [.number,.head.sha] | @tsv") || return 1
    read -r number head <<< "$pr_info"
    [[ $number =~ ^[0-9]+$ && $head =~ ^[0-9a-f]{40}$ ]] || return 1
    run=$(gh api "$repo/actions/workflows/ci.yml/runs" -X GET -f event=pull_request \
        -f status=success -f head_sha="$head" -F per_page=1 --jq '.workflow_runs[0].id // empty') || return 1
    [[ $run =~ ^[0-9]+$ ]] || return 1
    # The PR job name records the tested merge tree, including its base branch.
    job=$(gh api "$repo/actions/runs/$run/jobs?per_page=100" --jq \
        ".jobs[] | select(.name == \"msrv / $number / $tree\" and .conclusion == \"success\") | .id") || return 1
    [[ $job =~ ^[0-9]+$ ]] || return 1
    echo "Reusing the minimum-Rust check from PR #$number, run $run, for tree $tree." >&2
}

if [[ -n $base ]] && git cat-file -e "$base^{commit}" 2>/dev/null; then
    if docs_only "$base" && ! git diff --quiet "$base" HEAD; then
        route=docs
    elif [[ $GITHUB_EVENT_NAME == push ]] && already_tested; then
        route=verified-msrv
    fi
fi

printf 'route=%s\ntree=%s\n' "$route" "$tree" >> "$GITHUB_OUTPUT"
echo "CI route: $route"
