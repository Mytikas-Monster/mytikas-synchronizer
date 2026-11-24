mod archive;
mod github;
mod worker;

use anyhow::{Context, Result, bail};
use sqlx::{Connection, PgConnection};
use std::{env, fs, path::Path, time::Duration};

const HEARTBEAT: &str = "/tmp/mytikas-synchronizer-heartbeat";

async fn database(variable: &str) -> Result<PgConnection> {
    let url = env::var(variable).with_context(|| format!("missing {variable}"))?;
    Ok(tokio::time::timeout(Duration::from_secs(12), PgConnection::connect(&url)).await??)
}

#[tokio::main]
async fn main() -> Result<()> {
    match env::args().nth(1).as_deref() {
        Some("migrate") => {
            let mut db = database("ARCHIVE_DATABASE_URL").await?;
            sqlx::raw_sql(include_str!("schema.sql"))
                .execute(&mut db)
                .await?;
        }
        Some("preflight") => github::preflight().await?,
        Some("health") => {
            if fs::metadata(HEARTBEAT)?.modified()?.elapsed()? > Duration::from_secs(180) {
                bail!("worker heartbeat expired");
            }
        }
        Some("worker") | None => {
            let _ = fs::remove_file(HEARTBEAT);
            loop {
                if let Err(error) = cycle().await {
                    eprintln!("synchronization cycle failed: {error:#}");
                }
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
        }
        Some(_) => bail!("expected worker, migrate, preflight, or health"),
    }
    Ok(())
}

async fn cycle() -> Result<()> {
    let mut app = database("APP_DATABASE_URL").await?;
    let mut store = database("ARCHIVE_DATABASE_URL").await?;
    // A dedicated connection owns the lock. Dropping it also releases the lock on failure.
    let leader: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(702601358023571)")
        .fetch_one(&mut store)
        .await?;
    if leader {
        worker::pickup(&mut app, &mut store).await?;
        worker::advance_first(&mut app, &mut store).await?;
        worker::track_receipt(&mut app, &mut store).await?;
    }
    fs::write(Path::new(HEARTBEAT), b"ok\n")?;
    Ok(())
}
