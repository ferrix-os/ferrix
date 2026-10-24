#!/usr/bin/env bash
# GitHub settings for the develop -> main flow (docs/REPO-FLOW.md). Run by the
# owner, with `gh auth login` as an administrator of ferrix-os/ferrix. Nothing
# changes without --apply: without it, every call is printed instead.
#
#   tools/common/release/github-flow-setup.sh setup  [--apply]
#       Safe the moment it runs, while the fleet still lands on main:
#       repository merge settings and auto-merge, Actions' token and fork
#       rules, secret scanning, the security-reviewed label, and the develop
#       ruleset (outside pull requests need CI, the lane, the verdict and a
#       code owner's approval; the owner's account bypasses it, so the fleet's
#       direct pushes to develop go on).
#
#   tools/common/release/github-flow-setup.sh switch [--apply]
#       The moment the product owner moves the fleet's landings to develop:
#       main then moves only as a fast-forward with `CI result` (from the
#       GitHub Actions app), `flow/lane` and `certification/verdict` green on
#       the pushed commit -- in practice by the Promote workflow. Run it when
#       no agent is mid-landing. Running it again applies a changed rule set.
#
#   tools/common/release/github-flow-setup.sh undo-switch [--apply]
#       Puts main's ruleset back to what it was before `switch` (no deletion,
#       no force-push, nothing else).
#
#   tools/common/release/github-flow-setup.sh show
#       Prints what is set now.
#
# No second account and no GitHub App are needed. The consultant's verdict is
# the commit status `certification/verdict`, posted by the Flow and Promote
# workflows with their own token, or by the consultant with the owner's token
# (tools/common/release/cert-verdict.py --verdict). A status is not a review,
# so GitHub's rule that an author cannot approve their own pull request does
# not touch it, and an outside contributor, who has no write access, cannot
# post one.
set -euo pipefail
repo=ferrix-os/ferrix
actions_app=15368   # GitHub Actions' app id: required checks must come from it

cmd=${1:-show}; apply=0; [ "${2:-}" = --apply ] && apply=1

run() {
    if [ $apply = 1 ]; then "$@"; else printf 'would run:'; printf ' %q' "$@"; printf '\n'; fi
}
# gh api with a JSON body from stdin
api_json() {
    local method=$1 path=$2 body
    body=$(cat)
    if [ $apply = 1 ]; then gh api -X "$method" "$path" --input - <<<"$body" >/dev/null
    else echo "would run: gh api -X $method $path"; echo "$body" | sed 's/^/    /'; fi
}
ruleset_id() { gh api "repos/$repo/rulesets" --jq ".[] | select(.name == \"$1\") | .id"; }

checks_rule() {
    cat <<JSON
{"type": "required_status_checks", "parameters": {
   "strict_required_status_checks_policy": false,
   "do_not_enforce_on_create": false,
   "required_status_checks": [
     {"context": "CI result", "integration_id": $actions_app},
     {"context": "flow/lane"},
     {"context": "certification/verdict"}]}}
JSON
}

case $cmd in
show)
    gh api "repos/$repo" --jq '{allow_auto_merge, allow_merge_commit, allow_squash_merge, allow_rebase_merge, delete_branch_on_merge}'
    gh api "repos/$repo/actions/permissions/workflow"
    gh api "repos/$repo/actions/permissions/fork-pr-contributor-approval" 2>/dev/null || echo "(fork approval policy: not readable)"
    for id in $(gh api "repos/$repo/rulesets" --jq '.[].id'); do
        gh api "repos/$repo/rulesets/$id" --jq '{name, enforcement, conditions, rules: [.rules[].type], bypass_actors}'
    done ;;

setup)
    # Merge settings. Promotions merge with a merge commit, so develop stays an
    # ancestor of main and the next promotion is a clean merge; auto-merge is
    # what lets a pull request merge itself when its checks pass. A merged
    # pull request's branch is deleted, so promote/<sha> branches do not pile up.
    run gh api -X PATCH "repos/$repo" -F allow_auto_merge=true -F allow_merge_commit=true \
        -F allow_squash_merge=true -F allow_rebase_merge=false -F delete_branch_on_merge=true \
        -f merge_commit_title=PR_TITLE -f merge_commit_message=PR_BODY

    # Actions: workflows get a read-only token unless they ask for more (ci.yml
    # asks for nothing; flow.yml, promote.yml and nightly.yml name what they
    # write), and no workflow may approve a pull request.
    run gh api -X PUT "repos/$repo/actions/permissions/workflow" \
        -f default_workflow_permissions=read -F can_approve_pull_request_reviews=false
    # Every fork pull request waits for a maintainer before its workflows run
    # ("Require approval for all external contributors"), not only a first
    # one: a second pull request from the same account is no safer.
    run gh api -X PUT "repos/$repo/actions/permissions/fork-pr-contributor-approval" \
        -f approval_policy=all_external_contributors

    # Secret scanning with push protection, and Dependabot's alerts, which the
    # dependency review in ci.yml reads.
    api_json PATCH "repos/$repo" <<'JSON'
{"security_and_analysis": {"secret_scanning": {"status": "enabled"},
                          "secret_scanning_push_protection": {"status": "enabled"}}}
JSON
    run gh api -X PUT "repos/$repo/vulnerability-alerts"

    # The label a maintainer puts on an outside pull request after reading its
    # security review; flow-lane.py takes it off again on every push.
    run gh label create security-reviewed --repo "$repo" --color B60205 --force \
        --description "A maintainer read the security review of the current head"

    # develop: no deletion, no force-push, and a pull request needs CI, the
    # lane, the verdict and a code owner's approval of its last push. The
    # repository's administrators (the owner, and the fleet, which pushes as
    # the owner) bypass it: their landings go straight to develop, gated by
    # the gate pool, and reach main only through a promotion.
    id=$(ruleset_id develop || true)
    method=POST; path="repos/$repo/rulesets"; [ -n "$id" ] && { method=PUT; path="$path/$id"; }
    api_json $method "$path" <<JSON
{"name": "develop", "target": "branch", "enforcement": "active",
 "conditions": {"ref_name": {"include": ["refs/heads/develop"], "exclude": []}},
 "bypass_actors": [{"actor_type": "RepositoryRole", "actor_id": 5, "bypass_mode": "always"}],
 "rules": [
   {"type": "deletion"},
   {"type": "non_fast_forward"},
   {"type": "pull_request", "parameters": {
      "required_approving_review_count": 1,
      "require_code_owner_review": true,
      "dismiss_stale_reviews_on_push": true,
      "require_last_push_approval": true,
      "required_review_thread_resolution": false,
      "allowed_merge_methods": ["merge", "squash"]}},
   $(checks_rule)]}
JSON
    echo "setup: done. main is unchanged until 'switch'." ;;

switch)
    # main (customer, 2026-10-24: promotions open no pull request, so the
    # owner gets no review request per promotion): no deletion, fast-forward
    # only, and the three checks green on any pushed commit -- `CI result`
    # only from the GitHub Actions app, so only a commit CI passed reaches
    # main. Nobody bypasses it, the Promote workflow included. No
    # pull-request rule: GitHub refuses the Actions app as a bypass actor on a
    # repository ruleset, and the checks already hold everything that rule
    # held. Not strict: a promotion is a fast-forward by construction.
    id=$(ruleset_id "protected main")
    [ -n "$id" ] || { echo "no ruleset named 'protected main'"; exit 1; }
    api_json PUT "repos/$repo/rulesets/$id" <<JSON
{"name": "protected main", "target": "branch", "enforcement": "active",
 "conditions": {"ref_name": {"include": ["refs/heads/main"], "exclude": []}},
 "bypass_actors": [],
 "rules": [
   {"type": "deletion"},
   {"type": "non_fast_forward"},
   $(checks_rule)]}
JSON
    # Promote and Nightly run only with this variable set.
    run gh variable set FLOW_PROMOTE --repo "$repo" --body on
    echo "switch: done. Land on develop from now on (docs/REPO-FLOW.md)." ;;

undo-switch)
    run gh variable delete FLOW_PROMOTE --repo "$repo"
    id=$(ruleset_id "protected main")
    api_json PUT "repos/$repo/rulesets/$id" <<'JSON'
{"name": "protected main", "target": "branch", "enforcement": "active",
 "conditions": {"ref_name": {"include": ["refs/heads/main"], "exclude": []}},
 "bypass_actors": [],
 "rules": [{"type": "deletion"}, {"type": "non_fast_forward"}]}
JSON
    ;;

*) sed -n '2,33p' "$0"; exit 64 ;;
esac
