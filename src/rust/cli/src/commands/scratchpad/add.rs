//! Add scratchpad entry handler

use anyhow::Result;

/// Best-effort write-time provenance: `(branch, cwd, is_worktree)`. Each field
/// is `None` when undetectable — never fabricated (a "main" fallback here
/// would misattribute notes written outside a repo).
fn detect_origin() -> (Option<String>, Option<String>, Option<bool>) {
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    let branch =
        git_stdout(&["rev-parse", "--abbrev-ref", "HEAD"]).filter(|branch| branch != "HEAD");
    // A linked worktree's git dir lives under <main>/.git/worktrees/<name>.
    let worktree = git_stdout(&["rev-parse", "--git-dir"]).map(|dir| dir.contains("/worktrees/"));
    (branch, cwd, worktree)
}

/// Trimmed stdout of a `git` invocation, or `None` on any failure.
fn git_stdout(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

use crate::grpc::ensure_daemon_available;
use crate::grpc::proto::{EnqueueItemRequest, QueueType, RefreshSignalRequest};
use crate::output;

use super::client::resolve_tenant_id;

pub(super) async fn add_entry(
    content: String,
    title: Option<String>,
    tags: Option<String>,
    project: Option<String>,
) -> Result<()> {
    let tenant_id = resolve_tenant_id(project.as_deref())?;

    let tag_vec: Vec<String> = tags
        .map(|t| {
            t.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let mut payload = serde_json::json!({
        "content": content,
        "title": title,
        "tags": tag_vec,
        "source_type": "scratchpad",
    });
    // Write-time provenance: the CLI runs inside the user's checkout, so
    // branch/cwd/worktree are directly observable here (unlike the container
    // MCP server). Attribution only — reads stay branch-agnostic.
    {
        let obj = payload.as_object_mut().expect("payload is a JSON object");
        let (origin_branch, origin_cwd, origin_worktree) = detect_origin();
        if let Some(branch) = origin_branch {
            obj.insert("origin_branch".to_string(), serde_json::json!(branch));
        }
        if let Some(cwd) = origin_cwd {
            obj.insert("origin_cwd".to_string(), serde_json::json!(cwd));
        }
        if let Some(worktree) = origin_worktree {
            obj.insert("origin_worktree".to_string(), serde_json::json!(worktree));
        }
    }
    let payload_json = payload.to_string();

    let mut client = ensure_daemon_available().await?;

    let response = client
        .queue_write()
        .enqueue_item(EnqueueItemRequest {
            item_type: "text".to_string(),
            op: "add".to_string(),
            tenant_id: tenant_id.to_string(),
            collection: wqm_common::constants::COLLECTION_SCRATCHPAD.to_string(),
            payload_json,
            branch: "main".to_string(),
            metadata_json: None,
        })
        .await?
        .into_inner();

    // Signal daemon to process queue
    let request = RefreshSignalRequest {
        queue_type: QueueType::IngestQueue as i32,
        lsp_languages: vec![],
        grammar_languages: vec![],
    };
    let _ = client.system().send_refresh_signal(request).await;

    output::section("Scratchpad Entry Queued");
    output::kv("Queue ID", &response.queue_id);
    output::kv("Tenant", &tenant_id);
    if let Some(t) = &title {
        output::kv("Title", t);
    }
    if !tag_vec.is_empty() {
        output::kv("Tags", tag_vec.join(", "));
    }
    let preview = if content.len() > 80 {
        format!("{}...", &content[..77])
    } else {
        content
    };
    output::kv("Content", &preview);
    if !response.is_new {
        output::warning("Duplicate entry (already queued)");
    }

    Ok(())
}
