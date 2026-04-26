#!/usr/bin/env bash
# setup-branch-protection.sh — apply the branch-protection rules
# this project relies on.
#
# Run this once after making the repo public, and again any time
# you add or rename a required CI status check. It's idempotent:
# re-running with no changes is a no-op for everything except the
# API timestamp.
#
# What it enforces on `main`:
#
#   * Every change must arrive through a pull request — direct
#     pushes to `main` are blocked. (`required_pull_request_reviews`)
#   * CODEOWNERS-listed reviewers (the repo owner) are auto-
#     requested on every PR. (`require_code_owner_reviews`)
#   * Required status checks must pass before merge:
#       - check (stable)        from .github/workflows/ci.yml
#       - check (1.88)          from .github/workflows/ci.yml
#       - security              from .github/workflows/ci.yml
#       - codeql analyze (rust) from .github/workflows/codeql.yml
#     CI must also be up-to-date with `main` before merge
#     (`strict: true` — GitHub's "require branches to be up to
#     date" toggle).
#   * Linear history only — merge commits are blocked, only
#     squash- or rebase-merge is allowed via the matching repo
#     setting. Keeps `git log` readable.
#   * Force-push and branch deletion are blocked.
#   * Conversation resolution is required before merge — every
#     review thread must be resolved.
#
# What it deliberately *doesn't* enforce:
#
#   * `enforce_admins: false` — the maintainer (you) keeps the
#     emergency-override hatch. With one human on the project, an
#     "admin can't bypass" rule would be a tripwire, not a
#     safety net.
#   * `required_approving_review_count: 0` — GitHub does not let
#     PR authors approve their own PRs, so requiring an approval
#     count > 0 on a solo project would deadlock every PR you
#     write yourself. CODEOWNERS still auto-requests your review,
#     and only you have `write` access on a personal repo, so
#     "only you can merge" is enforced by the permission model
#     not by the count.
#
# Prerequisites:
#
#   * `gh` CLI authenticated as a user with admin rights on the
#     repo: `gh auth status` should show `nt2311-vn`.
#   * Run from the repo working tree (uses `git remote` to find
#     owner/repo).

set -euo pipefail

# ---- Resolve owner/repo from git remote -------------------------
remote_url=$(git config --get remote.origin.url)
# Strip both ssh (`git@github.com:owner/repo.git`) and https
# (`https://github.com/owner/repo.git`) variants.
slug=${remote_url#*github.com[:/]}
slug=${slug%.git}

if [[ -z "${slug}" || "${slug}" == "${remote_url}" ]]; then
  echo "error: could not parse owner/repo from remote: ${remote_url}" >&2
  exit 1
fi

echo "Applying branch protection to: ${slug} (branch: main)"
echo

# ---- Required status checks -------------------------------------
# `contexts` are matched by the *job's* "name:" field, not by
# workflow filename. If you rename a job in ci.yml or codeql.yml
# you must update both this script and any open PRs that targeted
# the old name.
read -r -d '' payload <<JSON || true
{
  "required_status_checks": {
    "strict": true,
    "contexts": [
      "check (stable)",
      "check (1.88)",
      "security",
      "codeql analyze (rust)"
    ]
  },
  "enforce_admins": false,
  "required_pull_request_reviews": {
    "dismiss_stale_reviews": true,
    "require_code_owner_reviews": true,
    "required_approving_review_count": 0,
    "require_last_push_approval": false
  },
  "restrictions": null,
  "required_linear_history": true,
  "allow_force_pushes": false,
  "allow_deletions": false,
  "block_creations": false,
  "required_conversation_resolution": true,
  "lock_branch": false,
  "allow_fork_syncing": true
}
JSON

gh api \
  --method PUT \
  -H "Accept: application/vnd.github+json" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  "repos/${slug}/branches/main/protection" \
  --input - <<<"${payload}"

echo
echo "✓ branch protection applied"
echo
echo "Verify in the UI: https://github.com/${slug}/settings/branches"
echo
echo "Recommended companion repo settings (apply once via the UI;"
echo "no API equivalent worth scripting):"
echo
echo "  * Settings → General → Pull Requests"
echo "      [x] Allow squash merging   (default merge type)"
echo "      [x] Allow rebase merging"
echo "      [ ] Allow merge commits    (turn OFF — keeps history linear)"
echo "      [x] Always suggest updating pull request branches"
echo "      [x] Automatically delete head branches"
echo
echo "  * Settings → Code security"
echo "      [x] Dependabot alerts"
echo "      [x] Dependabot security updates"
echo "      [x] Code scanning   (CodeQL is wired up automatically"
echo "          once .github/workflows/codeql.yml runs once)"
echo "      [x] Secret scanning + push protection"
echo "      [x] Private vulnerability reporting"
echo
echo "  * Settings → Actions → General"
echo "      Workflow permissions → Read repository contents (default)"
echo "      [x] Require approval for first-time contributors"
echo "      [x] Fork pull request workflows from outside collaborators"
echo "          → Require approval for all outside collaborators"
