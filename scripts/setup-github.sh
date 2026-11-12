#!/usr/bin/env bash
# Reproduces the GitHub repository configuration for bardesss/perchbar.
# Requires: gh (logged in as the repo owner), run from the repo root.
set -euo pipefail

REPO="bardesss/perchbar"

# Step 2: create the public repo and push main.
gh repo create "$REPO" --public \
  --description "Customizable live widgets on the Windows 11 taskbar" \
  --source . --push

# Step 3: repo settings.
gh api -X PATCH "repos/$REPO" \
  -F delete_branch_on_merge=true \
  -F allow_squash_merge=true \
  -F allow_merge_commit=false \
  -F allow_rebase_merge=false \
  -f squash_merge_commit_title=PR_TITLE \
  -f squash_merge_commit_message=PR_BODY \
  -F allow_auto_merge=true \
  -F has_wiki=false \
  -F has_projects=false

# Step 4: security features.
gh api -X PATCH "repos/$REPO" --input - <<'JSON'
{
  "security_and_analysis": {
    "secret_scanning": { "status": "enabled" },
    "secret_scanning_push_protection": { "status": "enabled" },
    "dependabot_security_updates": { "status": "enabled" }
  }
}
JSON
gh api -X PUT "repos/$REPO/vulnerability-alerts"
gh api -X PUT "repos/$REPO/private-vulnerability-reporting"

gh api -X PUT "repos/$REPO/actions/permissions/workflow" --input - <<'JSON'
{ "default_workflow_permissions": "read", "can_approve_pull_request_reviews": false }
JSON

# Step 5: branch ruleset for main. No bypass actors.
gh api -X POST "repos/$REPO/rulesets" --input - <<'JSON'
{
  "name": "protect-main",
  "target": "branch",
  "enforcement": "active",
  "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
  "bypass_actors": [],
  "rules": [
    { "type": "deletion" },
    { "type": "non_fast_forward" },
    { "type": "required_linear_history" },
    {
      "type": "pull_request",
      "parameters": {
        "required_approving_review_count": 0,
        "dismiss_stale_reviews_on_push": true,
        "require_code_owner_review": false,
        "require_last_push_approval": false,
        "required_review_thread_resolution": true
      }
    },
    {
      "type": "required_status_checks",
      "parameters": {
        "strict_required_status_checks_policy": true,
        "required_status_checks": [ { "context": "CI / build" } ]
      }
    }
  ]
}
JSON

# Step 6: verify.
gh api "repos/$REPO" --jq '{delete_branch_on_merge, allow_squash_merge, allow_merge_commit, sa: .security_and_analysis}'
gh api "repos/$REPO/rulesets" --jq '.[].name'

# Step 7: working branch for Plan 1.
git switch -c plan-1-core main

# CodeQL default setup. Kept last and non-fatal: the API rejects `rust`
# (unsupported), and `actions` is only accepted once workflow files exist on
# the default branch. Re-run this block after .github/workflows is on main.
gh api -X PATCH "repos/$REPO/code-scanning/default-setup" --input - <<'JSON' \
  || echo "CodeQL not configured: re-run this step after .github/workflows exists on the default branch." >&2
{ "state": "configured", "query_suite": "default", "languages": ["actions"] }
JSON
