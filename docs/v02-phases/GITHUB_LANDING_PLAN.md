# GitHub landing plan — FetchForge v0.2.0 (`audit-round` → GitHub `main`)

> For the user to approve. NOTHING in this plan has been executed:
> no remote was added, nothing was pushed, no force-push decision
> was made. The force-push question belongs to the user.

## 1. The situation (verified locally, 2026-11-21)

- Local clone `~/workspace/fetchforge`, branch `audit-round`: **51
  commits**, tip at the Phase 10 docs commit. Working tree clean.
- Local branch `main` (tip `b87a82b`, "Token pairing: stable
  extension ID...") is the v0.1.0 local history. `audit-round` is
  **48 commits ahead of it** and shares history with it
  (merge-base `b87a82b`). **Local `main` is NOT GitHub `main`.**
- GitHub `main` (repo `najeeb-BIM/fetchforge`) is a **single
  web-upload root commit, unrelated to the local history**. It holds
  the build #8 state (the working v0.1.0 installer lineage, edited
  in the GitHub web UI). The local clone has **no remote configured**,
  so the exact content of GitHub `main`'s root commit could not be
  inspected from this VM — the user (or an agent in his browser)
  should confirm what it contains before landing.
- The Windows CI workflow (`.github/workflows/windows-build.yml`)
  triggers **only on push to `main`** (plus manual
  `workflow_dispatch`). A push to any other branch does NOT build
  the installer automatically — but `workflow_dispatch` can run the
  workflow manually on any branch that exists on GitHub.
- Tags `v0.2.0-rc1`, `v0.2.0-rc2`, `v0.2.0-rc3` exist locally but
  point at old pre-audit-round commits (build #2/#3 era). They are
  NOT v0.2.0 release candidates of the current tree and must NOT be
  pushed as-is. Tag the landing commit `v0.2.0` only after the
  landing is approved and merged.

## 2. Options

### Option A — Force-push `audit-round` to GitHub `main`

```
git remote add origin https://github.com/najeeb-BIM/fetchforge.git
git push --force-with-lease origin audit-round:main
```

- **Destroys:** GitHub `main`'s root commit and its history (the
  web-upload lineage, builds #4–#8 as edited in the web UI). Once
  force-pushed, that history is gone from GitHub unless someone
  recorded the old commit SHA — no SHA is recorded anywhere in
  this repo. The build #8 installer artifacts already downloaded
  (in the user's Downloads) survive, but the source history does
  not.
- **Preserves:** everything in the local clone (all 51 commits,
  tags, the local `main` branch). Nothing local is lost.
- **Result:** `main` becomes the 51-commit audit-round history; the
  Windows CI runs on the push and builds the v0.2.0 installer.
- **Needs the user's explicit approval** — this is the decision the
  build has been waiting on. Do not do it on a general "land it".

### Option B — Push `audit-round` as a new branch, open a PR to `main`

```
git push origin audit-round:v0.2.0
# then open a PR v0.2.0 -> main in the GitHub web UI
```

- **Destroys:** nothing. No force-push anywhere.
- **Problem:** the histories are unrelated, so the PR diff is the
  entire tree and a merge would need `--allow-unrelated-histories`
  and manual resolution of essentially every file (both sides
  contain e.g. `index.html`, the manifests, the workflow, with
  different content). The merge result would be a tangle, not a
  clean v0.2.0. **Not recommended as a landing path.**
- CI does not auto-run on the branch push (only `main` triggers);
  use `workflow_dispatch` on the `v0.2.0` branch to build the
  installer without touching `main`.

### Option C — Squash the v0.2.0 tree onto GitHub `main` via PR (RECOMMENDED)

```
git remote add origin https://github.com/najeeb-BIM/fetchforge.git
git fetch origin
git checkout -b v0.2.0-landing origin/main
git checkout audit-round -- .        # take the v0.2.0 tree exactly
git add -A
git commit -m "v0.2.0: universal media downloading (see docs/v02-phases/)"
git push origin v0.2.0-landing
# open a PR v0.2.0-landing -> main; merge it in the web UI
```

- **Destroys:** nothing on GitHub. GitHub `main`'s root commit stays
  as the parent; v0.2.0 lands as one commit on top of it.
- **Preserves:** the full 51-commit history locally (and in the
  local tags). It is not on GitHub, but it is not lost — the local
  clone is the record, and the phase reports in
  `docs/v02-phases/` describe each step.
- **Result:** `main` is a clean, reviewable v0.2.0; the Windows CI
  runs when the PR merges. The PR diff is large (the whole tree
  vs the web-upload root) but it is a normal single-commit diff
  with no merge conflicts.
- **Caveat:** the squashed commit will show every file as changed
  vs the web-upload root (expected — the root is a snapshot, not a
  real ancestor). Review the PR file list before merging: it must
  be exactly the audit-round tree (79 files changed vs local main,
  +15,479/−380). If anything in the web-upload root is NOT in the
  local tree (e.g. a file added only via the web UI after the last
  sync at `41954a0`), it will be deleted by the squash — check
  for that first (see §3).

### Option D — Build the installer without landing (defer the decision)

```
git push origin audit-round:v0.2.0
# GitHub web UI: Actions -> Windows build -> Run workflow -> branch v0.2.0
```

- **Destroys:** nothing. GitHub `main` stays exactly as it is.
- **Result:** a v0.2.0 installer artifact is produced from the
  `v0.2.0` branch for testing on the user's Windows laptop, while
  the landing decision stays open. Use this if the user wants the
  installer verified on Windows BEFORE deciding how `main` is
  rewritten (the Windows build is still unverified — see
  `PHASE_10_TESTING.md` §6).
- This is the right first step if the user is unsure about Option C.

## 3. Pre-landing checks (do these before any option)

1. Confirm what GitHub `main`'s root commit contains that the local
   tree does not (open the repo in the browser, compare the file
   list with the local tree). Anything web-only must be preserved
   deliberately — either merged into the local tree first, or
   accepted as superseded.
2. Confirm the local tree builds on Windows: run Option D first
   (dispatch the workflow on a pushed branch) and install the
   resulting build on the laptop before rewriting `main`. The
   Windows compile/installer is the one gate Phase 10 could not
   verify.
3. Do NOT push the local `main` branch or the `v0.2.0-rc1/2/3`
   tags. Tag `v0.2.0` on the landing commit after the merge.

## 4. Recommendation

**Option C** (squash onto GitHub `main` via PR), preceded by
**Option D** (dispatch the workflow on a pushed `v0.2.0` branch and
verify the installer on Windows first). This lands a clean v0.2.0,
destroys no GitHub history, keeps the full 51-commit history in the
local clone, and verifies the Windows build before `main` moves.
**Option A (force-push) is not recommended**: it destroys the only
copy of the web-upload history for no benefit Option C does not
already give — unless the user explicitly wants `main`'s history to
be the 51-commit audit-round history, in which case say so in so
many words and Option A can be executed in one command.
