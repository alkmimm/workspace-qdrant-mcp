//! GraphServiceImpl struct definition, constructor and branch scoping.

use std::collections::HashSet;

use sqlx::SqlitePool;
use tonic::Status;
use workspace_qdrant_core::graph::branch_scope::{resolve_branch_scope, ALL_BRANCHES};
use workspace_qdrant_core::graph::{GraphScope, SharedGraphStore, SqliteGraphStore};

use crate::proto::GraphScopeProto;

/// GraphService implementation backed by SharedGraphStore.
pub struct GraphServiceImpl {
    pub(crate) graph_store: SharedGraphStore<SqliteGraphStore>,
    /// `state.db`, where branch membership lives (`tracked_files.branches`).
    /// Without it every answer is unscoped — tests that build a bare graph.
    pub(crate) state_pool: Option<SqlitePool>,
}

impl GraphServiceImpl {
    /// Create a new GraphService with a shared graph store handle.
    pub fn new(graph_store: SharedGraphStore<SqliteGraphStore>) -> Self {
        Self {
            graph_store,
            state_pool: None,
        }
    }

    /// Scope every answer to the asking branch, reading membership from `pool`.
    pub fn with_state_pool(mut self, pool: Option<SqlitePool>) -> Self {
        self.state_pool = pool;
        self
    }

    /// Resolve a request's branch to the generations it may see, plus the
    /// coverage block its response carries. Call BEFORE taking the store's read
    /// guard: the coverage count takes one too, and tokio's fair RwLock would
    /// deadlock a nested read behind a queued writer.
    pub(crate) async fn resolve_scope(
        &self,
        tenant_id: &str,
        branch: Option<&str>,
    ) -> Result<(GraphScope, GraphScopeProto), Status> {
        let Some(pool) = &self.state_pool else {
            return Ok((
                GraphScope::all(),
                GraphScopeProto {
                    branch: ALL_BRANCHES.to_string(),
                    ..Default::default()
                },
            ));
        };
        let resolved = resolve_branch_scope(pool, tenant_id, branch)
            .await
            .map_err(|e| Status::internal(format!("Branch scope query failed: {e}")))?;
        let extracted: HashSet<String> = self
            .graph_store
            .extracted_generations(tenant_id)
            .await
            .map_err(|e| Status::internal(format!("Graph coverage query failed: {e}")))?
            .into_iter()
            .map(|g| g.generation)
            .collect();
        let graphed = resolved
            .held
            .iter()
            .filter(|g| extracted.contains(*g))
            .count();
        Ok((
            resolved.scope,
            GraphScopeProto {
                branch: resolved.branch,
                indexed_files: resolved.held.len() as u32,
                graphed_files: graphed as u32,
            },
        ))
    }
}
