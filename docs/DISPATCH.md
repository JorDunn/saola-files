# Dispatcher runbook

How the main Claude session runs the board. Written for a session with no
memory of earlier ones: read this and `AGENTS.md`, then start at
[The loop](#the-loop).

## Where the work lives

-   Board: the "Saola" user Project,
    <https://github.com/users/JorDunn/projects/2> (project number `2`, owner
    `JorDunn`). Never create a second project.
-   Repos on the board: `JorDunn/saola-files` (this one) and
    `JorDunn/saola-theme`. The dispatcher implements saola-files tasks only;
    saola-theme is worked by the session named `saola-theme`.
-   Epics are milestones (`M0`–`M13`; milestone number order is work order)
    with sub-issues. Ordering between tasks is GitHub's "blocked by" link.
-   The Project's `Status` field: `Todo`, `In Progress`, `Done`.

## Labels

| Label                | Meaning                                                   |
| -------------------- | --------------------------------------------------------- |
| `area:*`             | Part of the code the task touches (`core`, `ui`, `ops`, `vfs`, …). Informational. |
| `model:sonnet`       | Implement with Sonnet. The default when no `model:*` label is set. |
| `model:opus`         | Implement with Opus.                                      |
| `solo`               | Touches most of the tree; runs with no other task in flight. |
| `lock:backend-trait` | Changes the `Backend` trait; never concurrent with another holder. |
| `lock:config`        | Changes config reading or writing; never concurrent with another holder. |
| `lock:keymap`        | Changes the keymap; never concurrent with another holder. |
| `needs-jordan`       | Waits on the maintainer. Never dispatched.                |
| `blocked:external`   | Waits on a crate built outside the Saola project. Never dispatched. |
| `epic`               | Parent issue that groups sub-issues. Never dispatched.    |
| `theme-request`      | Issue in saola-theme asking for a token, style or glyph.  |
| `bug`                | Defect, including a failed live GUI check.                |

## Ready queue

A task is ready when all of these hold:

-   it is open, in `JorDunn/saola-files`;
-   every `blockedBy` issue is `CLOSED` (cross-repo blockers included);
-   it has no assignee (assigned = in flight);
-   it is not an epic: `subIssuesSummary.total == 0` and no `epic` label;
-   it is not labelled `needs-jordan` or `blocked:external`.

Order: lowest milestone number first, then lowest issue number.

```bash
gh api graphql --paginate --slurp -f owner=JorDunn -f repo=saola-files -f query='
query($owner: String!, $repo: String!, $endCursor: String) {
  repository(owner: $owner, name: $repo) {
    issues(states: OPEN, first: 100, after: $endCursor) {
      pageInfo { hasNextPage endCursor }
      nodes {
        number
        title
        milestone { number title }
        assignees(first: 5) { nodes { login } }
        labels(first: 30) { nodes { name } }
        subIssuesSummary { total }
        blockedBy(first: 50) { nodes { state number repository { nameWithOwner } } }
      }
    }
  }
}' | jq -r '
  [.[].data.repository.issues.nodes[]]
  | map(select(
      (.blockedBy.nodes | all(.state == "CLOSED"))
      and (.assignees.nodes | length == 0)
      and (.subIssuesSummary.total == 0)
      and ([.labels.nodes[].name]
           | any(. == "needs-jordan" or . == "blocked:external" or . == "epic")
           | not)))
  | sort_by((.milestone.number // 999999), .number)
  | .[]
  | "#\(.number)\t\(.milestone.title // "-")\t\([.labels.nodes[].name] | join(","))\t\(.title)"'
```

Output: one line per ready task, `#<n>  <milestone>  <labels>  <title>`.

**Fallback** if native "blocked by" links cannot be written (for example
cross-repo): the issue body carries one line per blocker, exactly
`Blocked by: <owner>/<repo>#<n>`. For each candidate the query returned,
check those lines too and drop the task if any blocker is open:

```bash
n=123  # candidate issue number
gh issue view "$n" -R JorDunn/saola-files --json body -q .body \
  | grep -oE '^Blocked by: [A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+#[0-9]+' \
  | sed -E 's/^Blocked by: (.+)#([0-9]+)$/\1 \2/' \
  | while read -r repo num; do
      printf '%s#%s %s\n' "$repo" "$num" \
        "$(gh issue view "$num" -R "$repo" --json state -q .state)"
    done
# Any line ending in OPEN means the task is not ready.
```

## Concurrency

-   At most 3 tasks in flight.
-   A `solo` task starts only when nothing is in flight, and nothing else
    starts until it merges or stops.
-   Two tasks carrying the same `lock:*` label never run together.
-   When the queue holds more ready tasks than free slots, take them in queue
    order, skipping any that would break a rule above.

## The loop

Repeat until a [stop condition](#stop-conditions) holds:

1.  Run the ready query.
2.  Fill free slots under the concurrency rules (lifecycle below).
3.  As PRs go green and pass review, merge them and run their live checks.
4.  Re-run the query after every merge; a merge can unblock tasks.

## Task lifecycle

### 1. Claim

```bash
gh issue edit <n> -R JorDunn/saola-files --add-assignee @me
```

Set Status to `In Progress`. Field and option IDs can change, so look them
up each session rather than hard-coding them:

```bash
gh project view 2 --owner JorDunn --format json --jq .id      # project id
gh project field-list 2 --owner JorDunn --format json         # Status field id + option ids
gh api graphql -f query='{repository(owner:"JorDunn",name:"saola-files"){
  issue(number:<n>){projectItems(first:5){nodes{id project{number}}}}}}'  # item id
gh project item-edit --project-id <project-id> --id <item-id> \
  --field-id <status-field-id> --single-select-option-id <in-progress-id>
```

### 2. Implement

Spawn one implementation subagent per task, with `isolation: "worktree"`,
on the model from the task's `model:*` label (Sonnet if none). The brief
contains:

-   the issue number, title and full body (goal, acceptance criteria,
    validation, files likely touched, notes);
-   the "notes for the next task" from the PRs that closed this task's
    blockers;
-   branch `task/<n>-<slug>` from the latest `origin/main`;
-   "read `AGENTS.md` first; it is binding" and "use the
    `test-driven-development` skill";
-   the local gate, all of which must pass before pushing:

    ```bash
    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-features
    cargo build --all-features
    cargo build --no-default-features
    ```

-   PR rules: title is the Conventional Commit (it becomes the squash commit
    and the changelog line); body follows the PR template, with
    `Closes #<n>`; any `BREAKING CHANGE:` footer goes in the body;
-   "do not merge; do not run the live GUI check; do not touch other
    issues"; if a theme token, style or glyph is missing, stop and report it
    rather than styling locally;
-   the report to return: PR URL, gate results, live-check items, anything
    unfinished.

### 3. Review

Spawn a fresh review subagent (never the author) to run `/code-review` on
the PR against its issue. Send findings to the author agent to fix, then
review again. After two rounds without a clean review, label the PR and
issue `needs-jordan`, unassign the slot, and move on.

### 4. Merge

On a clean review and green required checks (`fmt`, `clippy`, `test`,
`build`, `pr-title`):

```bash
gh pr merge <pr> --squash --auto
```

If auto-merge is unavailable, wait for the checks and merge directly:

```bash
gh pr checks <pr> --watch --required --fail-fast && gh pr merge <pr> --squash
```

If the PR is behind `main` and checks need a re-run, `gh pr update-branch
<pr>` first. The squash commit takes the PR title and body; the issue
closes through `Closes #<n>`. Set Status to `Done` if the board did not.

### 5. Live GUI check

After merge, the dispatcher alone runs the AGENTS.md live verification
recipe on an up-to-date `main` build, one check at a time, covering the
PR's live-check items. Screenshots stay in the local scratchpad; never
attach them to GitHub (public repos). Write a worded result as a comment on
the closed issue.

If the check fails, file a `bug` task describing the failure in words, link
it as blocking the epic's remaining work where it matters, and continue.

## Theme requests

When a task needs something saola-theme does not have (token, style,
glyph, style-guide exception):

1.  File an issue in `JorDunn/saola-theme`, label `theme-request`, and add
    it to the board: `gh project item-add 2 --owner JorDunn --url <issue-url>`.
2.  Link it as a blocker of the saola-files task
    (`gh issue edit <n> -R JorDunn/saola-files --add-blocked-by <issue-url>`,
    or the `Blocked by:` body line if that fails).
3.  Send one message to the session named `saola-theme` naming the issue.
4.  Unassign the blocked task and keep working on other ready tasks.

A new style-guide exception also gets `needs-jordan`.

## External crates

When a task needs a crate that does not exist in pure Rust (see the
dependency rule in `AGENTS.md`):

1.  Write a brief under `docs/external-crates/` in the format in its
    `README.md`, as its own small `docs:` PR.
2.  Label the task `blocked:external` and note the brief in its body.
3.  Move on. The crate is built outside the Saola project; when it is
    published, remove the label.

## Always `needs-jordan`

Never dispatched until the maintainer decides:

-   the admin helper design and threat model;
-   a drag-and-drop approach that requires forking iced;
-   any new style-guide exception;
-   any change to the Boundaries section of `AGENTS.md`.

## Stop conditions

Stop the loop when any of these holds:

-   nothing is ready and nothing is in flight;
-   CI on `main` fails twice running;
-   only `needs-jordan` or `blocked:external` items remain.

The stop report contains:

-   merged PRs, each with its issue and one-line result;
-   blocked items and what each waits on (issue, theme request, external
    crate, maintainer decision);
-   live-check results, worded, including any `bug` tasks filed;
-   anything left in flight and its state.
