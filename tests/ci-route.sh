#!/usr/bin/env bash
# Exercise event ranges and minimum-Rust reuse with a synthetic Git history.
set -euo pipefail
script=$(realpath "$(dirname "$0")/../.github/scripts/ci-route.sh")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
cd "$work"
git init -q
git config user.name CI
git config user.email ci@example.invalid
mkdir -p docs/nested bin
export GITHUB_OUTPUT="$work/route" GITHUB_REPOSITORY=owner/repo
cat > bin/gh <<'MOCK'
#!/usr/bin/env bash
set -eu
case "$2" in
    */pulls) printf '9\t%040d\n' 1 ;;
    */ci.yml/runs) echo 20 ;;
    */jobs*) [[ $* == *"msrv / 9 / $TESTED_TREE"* ]] && echo 30 ;;
    *) exit 1 ;;
esac
MOCK
chmod +x bin/gh
export PATH="$work/bin:$PATH"

route() {
    export GITHUB_EVENT_NAME=$1 BEFORE_SHA=$2
    GITHUB_SHA=$(git rev-parse HEAD)
    export GITHUB_SHA
    : > "$GITHUB_OUTPUT"
    bash "$script" > /dev/null
    grep -Fxq "route=$3" "$GITHUB_OUTPUT" || { cat "$GITHUB_OUTPUT" >&2; exit 1; }
}

touch README.md
git add README.md && git commit -qm base
base=$(git rev-parse HEAD)
echo guide > docs/nested/setup.md
git add docs && git commit -qm docs
route pull_request '' docs
route push "$base" docs
route workflow_dispatch '' code
route merge_group '' code
touch Cargo.lock
git add Cargo.lock && git commit -qm dependencies
export TESTED_TREE=unverified
route pull_request '' code
route push "$base" code
previous=$(git rev-parse 'HEAD^')
route push "$previous" code
TESTED_TREE=$(git rev-parse 'HEAD^{tree}')
route push "$previous" verified-msrv
route push '' code
git mv Cargo.lock docs/renamed.md
git commit -qm rename
route pull_request '' code
echo 'CI routing checks passed.'
