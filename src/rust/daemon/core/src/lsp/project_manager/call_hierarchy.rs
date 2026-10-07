//! LSP call-hierarchy resolution: precise outgoing calls (callees).
//!
//! `textDocument/references` only finds usage *sites*; the call hierarchy
//! resolves the actual callee *definitions* (name + file + line). That lets the
//! graph bind a `CALLS` stub to the definition at that site instead of every
//! definition sharing its name. This is the community-recommended LSP method
//! for precise caller/callee relations.

use std::collections::BTreeMap;
use std::path::Path;

use super::{LanguageServerManager, ProjectLspResult};
use crate::graph::lsp_sites::LspSite;

/// A resolved call target: the callee symbol with its definition site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCall {
    /// Callee symbol name.
    pub name: String,
    /// Filesystem path of the callee's definition (`file://` prefix stripped).
    pub file: String,
    /// 0-indexed line of the callee's definition.
    pub line: u32,
}

impl LanguageServerManager {
    /// Resolve the outgoing calls (callees) of the symbol at `(line, column)`
    /// in `file` via `textDocument/prepareCallHierarchy` +
    /// `callHierarchy/outgoingCalls`.
    ///
    /// Returns an empty vec (never errors) when no server is ready, the symbol
    /// is not callable, or the server lacks call-hierarchy support — callers
    /// then fall back to tree-sitter name-stub edges.
    #[tracing::instrument(
        name = "lsp.outgoing_calls",
        skip_all,
        fields(file = %file.display(), line, column)
    )]
    pub async fn resolved_outgoing_calls(
        &self,
        file: &Path,
        line: u32,
        column: u32,
    ) -> ProjectLspResult<Vec<ResolvedCall>> {
        let (_key, server_instance) = self.find_server_for_file(file).await;
        let Some(instance) = server_instance else {
            return Ok(Vec::new());
        };
        let rpc_client = {
            let inst = instance.lock().await;
            inst.rpc_client()
        };

        // 1. prepareCallHierarchy at the symbol position.
        let prepare_params = serde_json::json!({
            "textDocument": { "uri": Self::file_to_uri(file) },
            "position": { "line": line, "character": column }
        });
        let prepared = match rpc_client
            .send_request("textDocument/prepareCallHierarchy", prepare_params)
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!(file = %file.display(), error = %e, "prepareCallHierarchy failed");
                return Ok(Vec::new());
            }
        };

        let Some(item) = prepared
            .result
            .as_ref()
            .and_then(|r| r.as_array())
            .and_then(|items| items.first())
            .cloned()
        else {
            return Ok(Vec::new());
        };

        // 2. outgoingCalls for the prepared item.
        let outgoing_params = serde_json::json!({ "item": item });
        let response = match rpc_client
            .send_request("callHierarchy/outgoingCalls", outgoing_params)
            .await
        {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!(file = %file.display(), error = %e, "outgoingCalls failed");
                return Ok(Vec::new());
            }
        };

        let calls = response
            .result
            .as_ref()
            .and_then(|r| r.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(parse_outgoing_call)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        tracing::debug!(file = %file.display(), count = calls.len(), "Resolved outgoing calls");
        Ok(calls)
    }

    /// Open `file` in its language server (`textDocument/didOpen`) so
    /// call-hierarchy requests are answered. Most servers only serve requests for
    /// OPEN documents — the missing didOpen was why the backfill resolved nothing.
    /// No-op when no server handles the file. Best-effort. Callers should open a
    /// file ONCE, resolve all its callers, then `close_document`, so the didOpen
    /// cost is amortised per-file rather than paid per-call.
    pub async fn open_document(&self, file: &Path) -> ProjectLspResult<()> {
        let (_key, server) = self.find_server_for_file(file).await;
        let Some(instance) = server else {
            return Ok(());
        };
        let rpc_client = {
            let inst = instance.lock().await;
            inst.rpc_client()
        };
        if let Ok(text) = tokio::fs::read_to_string(file).await {
            let _ = rpc_client
                .send_notification(
                    "textDocument/didOpen",
                    serde_json::json!({
                        "textDocument": {
                            "uri": Self::file_to_uri(file),
                            "languageId": lsp_language_id(file),
                            "version": 1,
                            "text": text,
                        }
                    }),
                )
                .await;
        }
        Ok(())
    }

    /// Close a document previously opened with `open_document`
    /// (`textDocument/didClose`) so the server does not accumulate open buffers.
    pub async fn close_document(&self, file: &Path) -> ProjectLspResult<()> {
        let (_key, server) = self.find_server_for_file(file).await;
        let Some(instance) = server else {
            return Ok(());
        };
        let rpc_client = {
            let inst = instance.lock().await;
            inst.rpc_client()
        };
        let _ = rpc_client
            .send_notification(
                "textDocument/didClose",
                serde_json::json!({ "textDocument": { "uri": Self::file_to_uri(file) } }),
            )
            .await;
        Ok(())
    }

    /// Wait until the server for `file` finishes (re)analyzing after a `didOpen`,
    /// so call-hierarchy is asked only once the document is analyzed.
    ///
    /// Dart's analysis server answers `callHierarchy/outgoingCalls` with an empty
    /// result while a freshly opened document is still being analyzed — a fixed
    /// short sleep was why the backfill resolved 0 Dart edges. After a brief
    /// settle (lets the analyzer flip `is_analyzing()` true), this polls until it
    /// clears or the 12s cap elapses. `is_analyzing()` tracks NON-indexing
    /// `$/progress` (the post-`didOpen` (re)analysis, e.g. Dart's "Analyzing…");
    /// background index/workspace-load progress is excluded (see process.rs), so
    /// servers whose only progress is indexing (typescript-language-server,
    /// pyright, and rust-analyzer/gopls once indexed) stay idle and this returns
    /// right after the settle — negligible cost for the fast path.
    pub async fn wait_for_analysis_idle(&self, file: &Path) {
        let (_key, server) = self.find_server_for_file(file).await;
        let Some(instance) = server else {
            return;
        };
        // Let the analyzer register the didOpen and flip to "analyzing" before we
        // poll for idle. Fast servers finish (or never signal) within this window.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(12);
        loop {
            let analyzing = { instance.lock().await.is_analyzing() };
            if !analyzing || tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
    }

    /// Wait for the server's INITIAL project analysis to complete after opening a
    /// representative file — paid ONCE per tenant during warm-up, before the
    /// resolve loop. Unlike [`wait_for_analysis_idle`] (a short per-file settle),
    /// this first waits for the analyzer to report it is busy (Dart requests its
    /// `ANALYZING` workDoneProgress token a few seconds after `didOpen`), then
    /// waits for that to clear. A large Dart monorepo takes ~1 min; call-hierarchy
    /// returns empty until it finishes, so this is what actually unblocks Dart.
    /// "Busy" is NON-indexing `$/progress` only (see process.rs), so a server
    /// still doing background indexing does not falsely satisfy phase 1 and pin
    /// phase 2 on unrelated work. No-op for servers that never report a
    /// non-indexing progress after `didOpen` (they answer immediately).
    pub async fn wait_for_initial_analysis(&self, file: &Path) {
        let (_key, server) = self.find_server_for_file(file).await;
        let Some(instance) = server else {
            return;
        };
        // Phase 1: wait for analysis to START (server reports busy). If it never
        // does within the window, the server needs no wait — return.
        let start_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
        let mut started = false;
        while tokio::time::Instant::now() < start_deadline {
            if instance.lock().await.is_analyzing() {
                started = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        if !started {
            return;
        }
        // Phase 2: wait for it to FINISH (generous — large projects analyse slowly).
        let done_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(180);
        while tokio::time::Instant::now() < done_deadline {
            if !instance.lock().await.is_analyzing() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

/// Best-effort LSP `languageId` for a file (used in `textDocument/didOpen`).
/// Servers key mostly on the document URI; this covers the languages we run.
fn lsp_language_id(file: &Path) -> &'static str {
    match file.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" => "python",
        "dart" => "dart",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hxx" => "cpp",
        "php" => "php",
        "rb" => "ruby",
        "cs" => "csharp",
        "kt" | "kts" => "kotlin",
        _ => "plaintext",
    }
}

/// Relativize an absolute path returned by the LSP server against the project
/// root, so the result matches the project-relative `file_path` the graph uses
/// to key nodes. Returns `None` for paths outside the project (stdlib/deps) and
/// for in-tree dependency directories (`node_modules/`, `site-packages/`),
/// which have no node in this tenant's graph. Separator-agnostic for
/// cross-platform/URI path encodings.
pub(crate) fn relativize_to_project(abs_file: &str, project_root: &str) -> Option<String> {
    let norm_file = abs_file.replace('\\', "/");
    let norm_root = project_root.replace('\\', "/");
    let norm_root = norm_root.trim_end_matches('/');
    let stripped = norm_file.strip_prefix(norm_root)?.trim_start_matches('/');
    if stripped.is_empty() || is_dependency_path(stripped) {
        None
    } else {
        Some(stripped.to_string())
    }
}

/// A project-relative path under a dependency directory is external code, not a
/// project symbol. The LSP server can resolve a call into a bundled package or
/// `.d.ts` that lives physically *under* the project root (so `strip_prefix`
/// keeps it), and the callee URI is often percent-encoded (`@types` →
/// `%40types`) so it never matches an on-disk node anyway. Skipping keeps the
/// graph project-scoped. This extends the out-of-project stdlib/deps skip this
/// function already performs (paths that fail `strip_prefix`, e.g. `/usr/lib`)
/// to deps that live inside the tree; the dependency-dir markers mirror the
/// external-dep paths recognized by `imports.rs::is_stdlib`
/// (`/node_modules/@types/`, `/site-packages/`, …). See issue #253.
fn is_dependency_path(rel: &str) -> bool {
    rel.contains("node_modules/") || rel.contains("site-packages/")
}

/// Where the server says each called name is defined: by callee name, the
/// project-relative file and line of every in-project definition site. A
/// name resolved only into stdlib/deps maps to no site — the call is not to
/// project code.
///
/// The site, not a node id, is what the graph keeps: the server never says
/// what KIND of symbol the callee is, and a method's id includes its class.
/// Guessing `Function` here minted a phantom node for every method callee;
/// the stub resolver binds each site to the definition that sits there
/// (`graph::lsp_sites`). Pure (no I/O).
pub(crate) fn call_sites_by_name(
    project_root: &str,
    calls: &[ResolvedCall],
) -> BTreeMap<String, Vec<LspSite>> {
    let mut by_name: BTreeMap<String, Vec<LspSite>> = BTreeMap::new();
    for call in calls {
        let sites = by_name.entry(call.name.clone()).or_default();
        let Some(rel) = relativize_to_project(&call.file, project_root) else {
            continue;
        };
        let site = (rel, call.line);
        if !sites.contains(&site) {
            sites.push(site);
        }
    }
    by_name
}

/// Parse one `CallHierarchyOutgoingCall` (its `to` item) into a `ResolvedCall`.
fn parse_outgoing_call(call: &serde_json::Value) -> Option<ResolvedCall> {
    let to = call.get("to")?;
    let name = to.get("name")?.as_str()?.to_string();
    let uri = to.get("uri")?.as_str()?;
    let file = uri.strip_prefix("file://").unwrap_or(uri).to_string();
    let line = to
        .get("selectionRange")
        .or_else(|| to.get("range"))
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|l| l.as_u64())
        .unwrap_or(0) as u32;
    Some(ResolvedCall { name, file, line })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_outgoing_call_extracts_resolved_target() {
        let call = serde_json::json!({
            "to": {
                "name": "add",
                "kind": 12,
                "uri": "file:///home/u/proj/src/lib.rs",
                "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 2, "character": 1 } },
                "selectionRange": { "start": { "line": 0, "character": 7 }, "end": { "line": 0, "character": 10 } }
            },
            "fromRanges": []
        });
        let resolved = parse_outgoing_call(&call).expect("should parse");
        assert_eq!(resolved.name, "add");
        assert_eq!(resolved.file, "/home/u/proj/src/lib.rs");
        // selectionRange wins over range for the definition line.
        assert_eq!(resolved.line, 0);
    }

    #[test]
    fn parse_outgoing_call_handles_missing_fields() {
        assert!(parse_outgoing_call(&serde_json::json!({})).is_none());
        // `to` present but no uri → None (cannot resolve a target file).
        assert!(parse_outgoing_call(&serde_json::json!({ "to": { "name": "x" } })).is_none());
    }

    #[test]
    fn relativize_strips_project_root() {
        assert_eq!(
            relativize_to_project("/home/u/proj/src/lib.rs", "/home/u/proj"),
            Some("src/lib.rs".to_string())
        );
        // Trailing slash on root, Windows-style separators.
        assert_eq!(
            relativize_to_project("C:\\dev\\proj\\src\\a.rs", "C:\\dev\\proj\\"),
            Some("src/a.rs".to_string())
        );
        // Outside the project (dependency/stdlib) → skipped.
        assert_eq!(
            relativize_to_project("/usr/lib/rustlib/std.rs", "/home/u/proj"),
            None
        );
    }

    #[test]
    fn relativize_skips_in_tree_dependency_dirs() {
        // node_modules lives physically under the project root, so strip_prefix
        // keeps it — but it is external code, not a project symbol (#253). The
        // '@' in @types arrives percent-encoded (%40) from the LSP URI.
        assert_eq!(
            relativize_to_project(
                "/home/u/proj/src/ts/node_modules/%40types/node/fs.d.ts",
                "/home/u/proj"
            ),
            None
        );
        // Python deps under an in-tree virtualenv are external too.
        assert_eq!(
            relativize_to_project(
                "/home/u/proj/.venv/lib/python3.12/site-packages/pkg/mod.py",
                "/home/u/proj"
            ),
            None
        );
    }

    fn call(name: &str, file: &str, line: u32) -> ResolvedCall {
        ResolvedCall {
            name: name.to_string(),
            file: file.to_string(),
            line,
        }
    }

    #[test]
    fn call_sites_are_project_relative_and_grouped_by_name() {
        let calls = vec![
            call("set", "/home/u/proj/lib/writes.dart", 2),
            // Two classes' `set` in one file: both sites, one name.
            call("set", "/home/u/proj/lib/writes.dart", 10),
            call("set", "/home/u/proj/lib/writes.dart", 10),
            call("add", "/home/u/proj/src/math.rs", 4),
        ];
        let sites = call_sites_by_name("/home/u/proj", &calls);
        assert_eq!(
            sites.get("set"),
            Some(&vec![
                ("lib/writes.dart".to_string(), 2),
                ("lib/writes.dart".to_string(), 10)
            ])
        );
        assert_eq!(
            sites.get("add"),
            Some(&vec![("src/math.rs".to_string(), 4)])
        );
    }

    #[test]
    fn a_name_resolved_only_outside_the_project_has_no_site() {
        // stdlib, and a bundled type-def under node_modules with the '@'
        // percent-encoded (#253): not project code, so no site — but the
        // name stays, so the stub is known not to be ours.
        let calls = vec![
            call("println", "/usr/lib/rust/std/macros.rs", 1),
            call(
                "readFileSync",
                "/home/u/proj/src/ts/node_modules/%40types/node/fs.d.ts",
                1,
            ),
        ];
        let sites = call_sites_by_name("/home/u/proj", &calls);
        assert_eq!(sites.get("println"), Some(&Vec::new()));
        assert_eq!(sites.get("readFileSync"), Some(&Vec::new()));
    }
}
