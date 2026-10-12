#!/usr/bin/env bash
# List the opencode commits in OLD..NEW that might matter to nth, as TSV:
#
#   sha  date  subject  pr  packages
#
#   new-commits.sh OLD NEW               the first-parent history of the range
#   new-commits.sh --merge SHA           the commits a branch merge brought in
#   new-commits.sh --all ...             keep what the filter would drop
#
# A commit is kept when its type is not noise (chore, docs, test, ...), its
# scope is neither noise (ci, release, ...) nor a dropped package, and it
# touches a package that is not product-only (console, stats, web, desktop,
# ...). Packages not named below are kept, so a new harness package
# shows up without editing this script.
#
# First-parent merges that bring in more than BIG commits are printed to
# stderr as "branch <sha> <count> <subject>": review those with --merge.
# Kept and dropped counts go to stderr.
set -euo pipefail

BIG=20
NOISE_TYPES='chore|docs?|test|ci|maint|style|build|zen|sync release versions'
NOISE_SCOPES='ci|release|vscode|update|updates|changelog|github|nix|infra|install|triage|i18n|go|www'
DROP_PACKAGES='app|console|containers|desktop|docs|effect-drizzle-sqlite|effect-sqlite-node|enterprise|function|gui-extensions|http-recorder|httpapi-codegen|identity|posts|script|slack|stats|storybook|ui|web'

# The clone lives in the main checkout, also when this runs in a worktree.
ROOT="$(dirname "$(git rev-parse --path-format=absolute --git-common-dir)")"
REF="$ROOT/refs/opencode"

all=false
merge=""
args=()
while [[ $# -gt 0 ]]; do
  case "$1" in
  --all) all=true ;;
  --merge) merge="$2"; shift ;;
  *) args+=("$1") ;;
  esac
  shift
done

if [[ -n "$merge" ]]; then
  range=("$merge^1..$merge^2")
  walk=()
elif [[ ${#args[@]} -eq 2 ]]; then
  range=("${args[0]}..${args[1]}")
  walk=(--first-parent)
else
  echo "usage: new-commits.sh [--all] OLD NEW | [--all] --merge SHA" >&2
  exit 2
fi

if [[ -z "$merge" ]]; then
  git -C "$REF" log "${walk[@]}" --merges --format='%H %s' "${range[@]}" |
    while read -r sha subject; do
      count=$(git -C "$REF" rev-list --count "$sha^1..$sha^2")
      if ((count > BIG)); then
        echo "branch ${sha:0:10} $count $subject" >&2
      fi
    done
fi

kept=0
dropped=0
while IFS=$'\t' read -r sha date subject; do
  packages=$(git -C "$REF" diff-tree --no-commit-id --name-only -r "$sha" |
    sed -nE 's#^packages/([^/]+)/.*#\1#p' | sort -u)
  wanted=$(grep -vxE "$DROP_PACKAGES" <<<"$packages" || true)
  if ! $all; then
    scope=$(sed -nE 's/^[a-z]+\(([^)]*)\).*/\1/p' <<<"$subject")
    if grep -qiE "^($NOISE_TYPES)(\(|:|!| )" <<<"$subject" ||
      grep -qxE "$NOISE_SCOPES|$DROP_PACKAGES" <<<"$scope" ||
      [[ -z "$wanted" ]]; then
      dropped=$((dropped + 1))
      continue
    fi
  fi
  pr=$(sed -nE 's/.*\(#([0-9]+)\)$/\1/p' <<<"$subject")
  printf '%s\t%s\t%s\t%s\t%s\n' "${sha:0:10}" "$date" "$subject" "$pr" "$(paste -sd, <<<"$packages")"
  kept=$((kept + 1))
done < <(git -C "$REF" log "${walk[@]}" --no-merges --date=short --format=$'%H\t%ad\t%s' "${range[@]}")

echo "kept $kept, dropped $dropped" >&2
