use crate::github::{self, Progress};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, Row};

/// Commit immutable bytes in the archive database before clearing the web outbox.
pub async fn pickup(app: &mut PgConnection, store: &mut PgConnection) -> Result<()> {
    let Some(row) = sqlx::query(
        "SELECT id,version,notes,sha256,archive FROM addon_submissions
         WHERE status='queued' ORDER BY id LIMIT 1",
    )
    .fetch_optional(&mut *app)
    .await?
    else {
        return Ok(());
    };
    let id: i64 = row.try_get("id")?;
    let version: &str = row.try_get("version")?;
    let notes: &str = row.try_get("notes")?;
    let hash: &str = row.try_get("sha256")?;
    let bytes: &[u8] = row.try_get("archive")?;
    if format!("{:x}", Sha256::digest(bytes)) != hash {
        return block(
            app,
            id,
            "Queued archive differs from its submitted checksum.",
        )
        .await;
    }
    let mut tx = store.begin().await?;
    sqlx::query(
        "INSERT INTO archives(submission_id,version,notes,sha256,archive)
         VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(version)
    .bind(notes)
    .bind(hash)
    .bind(bytes)
    .execute(&mut *tx)
    .await?;
    let stored: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM archives WHERE submission_id=$1 AND version=$2 AND notes=$3 AND sha256=$4 AND archive=$5",
    ).bind(id).bind(version).bind(notes).bind(hash).bind(bytes).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    let Some(archive_id) = stored else {
        return block(
            app,
            id,
            "An incompatible archive already reserves this submission or version.",
        )
        .await;
    };
    sqlx::query(
        "UPDATE addon_submissions SET archive_id=$2, archive=NULL, status='stored', detail=NULL
                 WHERE id=$1 AND status='queued'",
    )
    .bind(id)
    .bind(archive_id)
    .execute(app)
    .await?;
    Ok(())
}

async fn block(app: &mut PgConnection, id: i64, reason: &str) -> Result<()> {
    sqlx::query("UPDATE addon_submissions SET status='blocked', detail=$2 WHERE id=$1")
        .bind(id)
        .bind(reason)
        .execute(app)
        .await?;
    Ok(())
}

pub async fn advance_first(app: &mut PgConnection, store: &mut PgConnection) -> Result<()> {
    let Some(row) = sqlx::query(
        "SELECT id,archive_id,version,notes,sha256,pr_number FROM addon_submissions
         WHERE status IN ('stored','review') ORDER BY id LIMIT 1",
    )
    .fetch_optional(&mut *app)
    .await?
    else {
        return Ok(());
    };
    let id: i64 = row.try_get("id")?;
    let archive_id: i64 = row.try_get("archive_id")?;
    let version: &str = row.try_get("version")?;
    let notes: &str = row.try_get("notes")?;
    let hash: &str = row.try_get("sha256")?;
    let stored = sqlx::query("SELECT archive,pr_number FROM archives WHERE id=$1 AND submission_id=$2 AND version=$3 AND notes=$4 AND sha256=$5")
        .bind(archive_id).bind(id).bind(version).bind(notes).bind(hash).fetch_optional(&mut *store).await?
        .context("archive missing or database pair inconsistent; restore matching backups")?;
    let bytes: &[u8] = stored.try_get("archive")?;
    if format!("{:x}", Sha256::digest(bytes)) != hash {
        return block(
            app,
            id,
            "Stored archive does not match its recorded digest.",
        )
        .await;
    }
    let pr: Option<i64> = stored.try_get("pr_number")?;
    let progress = match github::advance(
        id,
        version,
        notes,
        hash,
        bytes,
        pr.or(row.try_get("pr_number")?),
    )
    .await
    {
        Ok(progress) => progress,
        Err(err) => {
            eprintln!("submission {id}: {err:#}");
            sqlx::query("UPDATE addon_submissions SET detail='GitHub synchronization is retrying. See synchronizer logs.' WHERE id=$1")
                .bind(id).execute(app).await?;
            return Ok(());
        }
    };
    let (pr, status, detail) = match progress {
        Progress::Blocked(reason) => return block(app, id, &reason).await,
        Progress::Open(pr) => (pr, "review", None),
        Progress::Waiting(pr, reason) => (pr, "review", Some(reason)),
        Progress::Closed(pr) => (pr, "closed", None),
        Progress::Published {
            pr,
            commit,
            release_id,
        } => {
            sqlx::query(
                "UPDATE archives SET pr_number=$2,merge_commit=$3,release_id=$4 WHERE id=$1",
            )
            .bind(archive_id)
            .bind(pr)
            .bind(commit)
            .bind(release_id)
            .execute(store)
            .await?;
            return publish(app, id, version, notes, hash, bytes, pr).await;
        }
    };
    sqlx::query("UPDATE archives SET pr_number=$2 WHERE id=$1 AND pr_number IS DISTINCT FROM $2")
        .bind(archive_id)
        .bind(pr)
        .execute(store)
        .await?;
    sqlx::query(
        "UPDATE addon_submissions SET pr_number=$2,status=$3,detail=$4 WHERE id=$1
                 AND (pr_number,status,detail) IS DISTINCT FROM ($2,$3,$4)",
    )
    .bind(id)
    .bind(pr)
    .bind(status)
    .bind(detail)
    .execute(app)
    .await?;
    Ok(())
}

async fn publish(
    app: &mut PgConnection,
    id: i64,
    version: &str,
    notes: &str,
    hash: &str,
    bytes: &[u8],
    pr: i64,
) -> Result<()> {
    let mut tx = app.begin().await?;
    sqlx::query(
        "INSERT INTO addon_releases(version,published_at,notes,sha256,archive)
                 VALUES($1,now(),$2,$3,$4) ON CONFLICT(version) DO NOTHING",
    )
    .bind(version)
    .bind(notes)
    .bind(hash)
    .bind(bytes)
    .execute(&mut *tx)
    .await?;
    let identical: bool =
        sqlx::query_scalar("SELECT sha256=$2 AND archive=$3 FROM addon_releases WHERE version=$1")
            .bind(version)
            .bind(hash)
            .bind(bytes)
            .fetch_one(&mut *tx)
            .await?;
    if identical {
        sqlx::query(
            "UPDATE addon_submissions SET status='published',pr_number=$2,detail=NULL WHERE id=$1",
        )
        .bind(id)
        .bind(pr)
        .execute(&mut *tx)
        .await?;
    } else {
        block(
            &mut tx,
            id,
            "Member download already contains different bytes for this version.",
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn track_receipt(app: &mut PgConnection, store: &mut PgConnection) -> Result<()> {
    let Some(row) = sqlx::query("SELECT id,submission_id,version,sha256,curseforge_file_id FROM archives
        WHERE release_id IS NOT NULL AND (receipt_checked_at IS NULL OR receipt_checked_at < now()-interval '5 minutes')
          AND curseforge_file_id IS NULL
        ORDER BY receipt_checked_at NULLS FIRST,id LIMIT 1").fetch_optional(&mut *store).await? else { return Ok(()) };
    let id: i64 = row.try_get("id")?;
    let submission: i64 = row.try_get("submission_id")?;
    sqlx::query("UPDATE archives SET receipt_checked_at=now() WHERE id=$1")
        .bind(id)
        .execute(&mut *store)
        .await?;
    match github::receipt(row.try_get("version")?, row.try_get("sha256")?).await {
        Ok(Some(file)) => {
            let updated = sqlx::query("UPDATE addon_submissions SET curseforge_file_id=$2 WHERE id=$1 AND archive_id=$3 AND status='published'")
                .bind(submission).bind(file).bind(id).execute(app).await?;
            ensure!(
                updated.rows_affected() == 1,
                "published submission is missing"
            );
            sqlx::query("UPDATE archives SET curseforge_file_id=$2 WHERE id=$1")
                .bind(id)
                .bind(file)
                .execute(store)
                .await?;
        }
        Err(err) => eprintln!("receipt for submission {submission}: {err:#}"),
        Ok(None) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing to a disposable PostgreSQL database"]
    async fn durable_handoff_and_member_publication() -> Result<()> {
        let url = std::env::var("TEST_DATABASE_URL")?;
        let mut app = PgConnection::connect(&url).await?;
        let mut store = PgConnection::connect(&url).await?;
        // Separate sessions and temporary tables reproduce the two commit boundaries.
        sqlx::raw_sql(
            "CREATE TEMP TABLE addon_submissions (
            id bigint PRIMARY KEY, archive_id bigint, version text, notes text, sha256 text,
            archive bytea, status text, detail text, pr_number bigint, curseforge_file_id bigint);
            CREATE TEMP TABLE addon_releases(version text PRIMARY KEY, published_at timestamptz,
            notes text, sha256 text, archive bytea);",
        )
        .execute(&mut app)
        .await?;
        sqlx::raw_sql(
            &include_str!("schema.sql").replace("CREATE TABLE IF NOT EXISTS", "CREATE TEMP TABLE"),
        )
        .execute(&mut store)
        .await?;
        let bytes = b"immutable bytes used by the handoff";
        let digest = format!("{:x}", Sha256::digest(bytes));
        sqlx::query(
            "INSERT INTO addon_submissions(id,version,notes,sha256,archive,status)
            VALUES(11,'2.0.0','Release notes',$1,$2,'queued')",
        )
        .bind(&digest)
        .bind(&bytes[..])
        .execute(&mut app)
        .await?;
        // Replay a restart after the archive commit and before acknowledging the outbox.
        sqlx::query(
            "INSERT INTO archives(submission_id,version,notes,sha256,archive)
            VALUES(11,'2.0.0','Release notes',$1,$2)",
        )
        .bind(&digest)
        .bind(&bytes[..])
        .execute(&mut store)
        .await?;
        pickup(&mut app, &mut store).await?;
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT archive IS NULL AND archive_id=1 AND status='stored' FROM addon_submissions"
            )
            .fetch_one(&mut app)
            .await?
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM archives")
                .fetch_one(&mut store)
                .await?,
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM addon_releases")
                .fetch_one(&mut app)
                .await?,
            0
        );

        for _ in 0..2 {
            publish(&mut app, 11, "2.0.0", "Release notes", &digest, bytes, 77).await?;
        }
        assert_eq!(
            sqlx::query_scalar::<_, Vec<u8>>("SELECT archive FROM addon_releases")
                .fetch_one(&mut app)
                .await?,
            bytes
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM addon_submissions")
                .fetch_one(&mut app)
                .await?,
            "published"
        );
        // A mismatched existing member release stays intact and the conflict is visible.
        sqlx::query("UPDATE addon_releases SET archive='wrong'")
            .execute(&mut app)
            .await?;
        publish(&mut app, 11, "2.0.0", "Release notes", &digest, bytes, 77).await?;
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM addon_submissions")
                .fetch_one(&mut app)
                .await?,
            "blocked"
        );
        assert_eq!(
            sqlx::query_scalar::<_, Vec<u8>>("SELECT archive FROM addon_releases")
                .fetch_one(&mut app)
                .await?,
            b"wrong"
        );
        // Inconsistent database pair must preserve incoming bytes for operator recovery.
        sqlx::query("UPDATE addon_submissions SET id=22,status='queued',archive=$1")
            .bind(&bytes[..])
            .execute(&mut app)
            .await?;
        pickup(&mut app, &mut store).await?;
        assert!(
            sqlx::query_scalar::<_, bool>(
                "SELECT archive IS NOT NULL AND status='blocked' FROM addon_submissions"
            )
            .fetch_one(&mut app)
            .await?
        );
        Ok(())
    }
}
