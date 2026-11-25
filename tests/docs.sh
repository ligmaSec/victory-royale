#!/usr/bin/env bash
# Check the tracked documentation layout in both local checkouts and CI.
set -euo pipefail
shopt -s nocasematch
cd "$(git rev-parse --show-toplevel)"

git ls-files --error-unmatch -- README.md >/dev/null
while IFS= read -r -d '' path; do
    case "$path" in
        readme.md|*/readme.md) ;;
        readme.*.md|*/readme.*.md|readme-*.md|*/readme-*.md|readme_*.md|*/readme_*.md)
            echo "Keep one English README per directory. Found $path" >&2
            exit 1
            ;;
    esac
    if [[ $path =~ ^(languages|translations)/ || $path =~ ^docs/([a-z]{2}[-_][a-z]{2}|pt|id)/ ]]; then
        echo "Keep English documentation in the main documentation tree. Found $path" >&2
        exit 1
    fi
done < <(git ls-files -z -- ':(icase)*.md')
