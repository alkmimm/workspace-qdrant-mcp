//! Samples how far each watched project's main checkout of its trunk trails
//! `origin/<trunk>` (`git::main_checkout_lag`) into
//! `memexd_main_checkout_behind_commits`, and says so in the log when a
//! project falls past the warning line. Git reads only: the daemon never
//! fetches and never moves a working tree.

use std::collections::HashMap;
use std::path::Path;

use sqlx::{Row, SqlitePool};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use workspace_qdrant_core::git::main_checkout_lag;
use workspace_qdrant_core::monitoring::checkout_lag_metrics::{
    publish_checkout_lag, CheckoutLagSample,
};

/// Seconds between passes: checkouts move on the scale of hours.
const INTERVAL_SECS: u64 = 300;

/// Lag, in commits, from which a project is logged as stale (the MCP's
/// `WQM_CHECKOUT_BEHIND_WARN_COMMITS` default).
const WARN_BEHIND: usize = 20;

/// A watched project's main folder.
struct Project {
    tenant_id: String,
    path: String,
}

/// Spawn the sampler.
pub fn start(pool: SqlitePool) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(INTERVAL_SECS));
        // Last behind count logged as stale, per tenant: log on change only.
        let mut warned: HashMap<String, usize> = HashMap::new();
        info!(
            interval_secs = INTERVAL_SECS,
            "Main checkout lag sampler started"
        );
        loop {
            interval.tick().await;
            match main_folders(&pool).await {
                Ok(projects) => sample(projects, &mut warned).await,
                Err(e) => debug!("checkout lag: list projects: {e}"),
            }
        }
    })
}

async fn main_folders(pool: &SqlitePool) -> Result<Vec<Project>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT tenant_id, path FROM watch_folders \
         WHERE collection = 'projects' AND enabled = 1 AND is_worktree = 0 \
           AND parent_watch_id IS NULL",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Project {
            tenant_id: r.get("tenant_id"),
            path: r.get("path"),
        })
        .collect())
}

async fn sample(projects: Vec<Project>, warned: &mut HashMap<String, usize>) {
    let measured = tokio::task::spawn_blocking(move || {
        projects
            .into_iter()
            .filter_map(|p| main_checkout_lag(Path::new(&p.path)).map(|lag| (p, lag)))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();

    let names: Vec<String> = measured
        .iter()
        .map(|(p, _)| {
            Path::new(&p.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&p.path)
                .to_string()
        })
        .collect();
    let samples: Vec<CheckoutLagSample<'_>> = measured
        .iter()
        .zip(&names)
        .map(|((p, lag), name)| CheckoutLagSample {
            tenant_id: &p.tenant_id,
            project: name,
            trunk: &lag.trunk,
            behind: lag.behind,
        })
        .collect();
    publish_checkout_lag(&samples);

    for ((p, lag), name) in measured.iter().zip(&names) {
        if lag.behind < WARN_BEHIND {
            warned.remove(&p.tenant_id);
        } else if warned.insert(p.tenant_id.clone(), lag.behind) != Some(lag.behind) {
            warn!(
                project = %name,
                trunk = %lag.trunk,
                behind = lag.behind,
                ahead = lag.ahead,
                "Main checkout of the trunk is behind origin: the trunk's index follows that \
                 checkout, so it holds old code until someone pulls"
            );
        }
    }
}
