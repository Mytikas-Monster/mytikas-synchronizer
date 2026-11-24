# Mytikas Synchronizer

This Rust worker takes official OlympusMytikas submissions from the guild website, stores the original ZIP in a dedicated PostgreSQL database, and opens a review PR from the Laymer fork. A successful merge and matching addon tree allow the worker to publish a stable GitHub release containing the administrator's original bytes. The existing OlympusMytikas release workflow submits the ZIP to CurseForge.

## Deployment

Run the reviewed web migration before starting the worker. Provision a separate persistent PostgreSQL 18 database for archives. Use the deployment helper and paired backup procedure shipped with [the web application](https://github.com/Mytikas-Monster/mytikas). Pin the container image to the digest published by this repository's main-branch workflow.

| Setting | Operator input |
| --- | --- |
| `APP_DATABASE_URL` | Web database URL for the restricted submission reader and release writer |
| `ARCHIVE_DATABASE_URL` | Dedicated archive database URL. Use its owner for migration and the restricted service role for the worker. |
| `GITHUB_APP_ID` | Numeric GitHub App ID |
| `GITHUB_APP_PRIVATE_KEY_FILE` | Path to the read-only RSA private-key mount |
| `GITHUB_FORK_INSTALLATION_ID` | Installation on the selected `Laymer/OlympusMytikas` repository |
| `GITHUB_UPSTREAM_INSTALLATION_ID` | Installation on the selected `Mytikas-Monster/OlympusMytikas` repository |

Create a GitHub App that can be installed on both owners. Select only the addon repository on each installation. The service needs Contents write on the fork for contribution branches, and Contents write, Pull requests write and Actions read on the upstream for PRs, verified workflow runs, tags and releases. Webhooks are optional. Store the private key outside the repositories, images and transfer archives. The CurseForge credential belongs to the upstream release workflow.

Run `mytikas-synchronizer migrate` with the archive migration credential. Grant the runtime roles, run `mytikas-synchronizer preflight` to check GitHub access, and start `mytikas-synchronizer worker`. It needs outbound HTTPS to GitHub and connections to both databases. Use verified TLS for remote databases. The image runs as a non-root user and needs a writable `/tmp` for its heartbeat. The `health` command checks the age of the last completed database polling cycle.

## Recovery

The app transaction reserves the version and queues the ZIP. The synchronizer verifies its SHA-256, commits the original bytes under a unique incremental archive ID, and then acknowledges the handoff. An interruption between the two commits reuses the stored archive. A PostgreSQL advisory lock coordinates worker replicas. PR review is serialized, with publication tied to the actual merged files and successful upstream checks.

The admin submission view shows the PR, release and CurseForge file ID as they become available. A closed PR ends that submission. For a blocked version, read the status detail and inspect the matching PR, tag and archive. Use a new version for changed bytes. Restore the two databases from a matched backup if their records disagree. GitHub availability and uncertain write responses are retried through the same branch, PR, tag and release. A GitHub release may precede the CurseForge receipt. Follow the [addon publisher's recovery instructions](https://github.com/Mytikas-Monster/OlympusMytikas#recover-a-release) if its workflow fails.

If the merged commit failed Addon behavior because of a transient CI problem, rerun that workflow on the same commit. After it passes, an operator can resume the blocked submission using an application database session and the recorded submission ID. Run this only after confirming the PR, commit and passing workflow in GitHub. The worker repeats its tree and check verification before publication.

```sql
UPDATE addon_submissions SET status = 'review', detail = NULL
WHERE id = :submission_id AND status = 'blocked'
  AND archive_id IS NOT NULL AND pr_number IS NOT NULL
  AND detail LIKE 'Addon behavior did not pass on the merged commit.%';
```

## Contributor checks

Fork the organization repository and open feature PRs from the fork. Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked`. Set `TEST_DATABASE_URL` to a disposable PostgreSQL database and run `cargo test --locked -- --ignored` to exercise the two-database handoff and member release transaction.
