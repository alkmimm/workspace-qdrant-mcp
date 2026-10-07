//! Gate 0 per-project ignore file matcher (.gitignore + .wqmignore).
//!
//! Reads `.gitignore` and/or `.wqmignore` files from a scan directory and
//! produces a matcher that can be queried for individual paths.
//!
//! Both files use standard gitignore syntax (powered by the `ignore` crate).
//! `.wqmignore` lets users add wqm-specific exclusions without touching the
//! project's `.gitignore`.
//!
//! ## .wqmignore negation (re-inclusion) syntax
//!
//! `.wqmignore` supports two equivalent syntaxes for re-including paths that
//! `.gitignore` excludes:
//!
//! - **Canonical**: `!pattern` — standard gitignore negation syntax
//! - **Legacy alias**: `- pattern` (dash space) — accepted for backward compatibility
//!
//! Both syntaxes are functionally identical: they cause the daemon to index a
//! path even when `.gitignore` excludes it. Use `!pattern` in new `.wqmignore`
//! files; `- pattern` continues to work for existing files.
//!
//! Note: re-inclusions are applied with a separate high-priority matcher, which
//! means they can override directory-level exclusions — something standard
//! gitignore `!` cannot do on its own.
//!
//! ## Priority rules
//!
//! `.wqmignore` always takes precedence over `.gitignore`:
//! - Both ignore → ignored
//! - Both re-include → not ignored
//! - `.gitignore` ignores, `.wqmignore` re-includes → **not ignored**
//! - `.gitignore` re-includes, `.wqmignore` ignores → **ignored**
//!
//! Resolution order:
//! 1. If `.wqmignore` re-includes the path → **not ignored** (overrides gitignore)
//! 2. If `.gitignore` or `.wqmignore` exclusions match → **ignored**
//! 3. Otherwise → **not ignored**

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;
use tracing::warn;

/// Per-directory matcher for `.gitignore` and `.wqmignore` (Gate 0).
///
/// Build once per `scan_directory_single_level` call via [`ProjectIgnoreMatcher::for_dir`],
/// then test every entry with [`ProjectIgnoreMatcher::is_ignored`] before
/// applying any other exclusion rules.
pub struct ProjectIgnoreMatcher {
    /// Per-ancestor-directory exclusion matchers (`.gitignore` + `.wqmignore`
    /// exclusion lines), in root→leaf order. Each layer is anchored AT the
    /// directory containing its ignore file — exactly like git. A single
    /// root-anchored `GitignoreBuilder` fed nested files used to mis-scope
    /// them both ways: a slash-containing negation (`!common/**/*.proto` in
    /// `proto/.gitignore`) resolved against the PROJECT root and went inert,
    /// while a bare glob (`*.proto`) leaked out of `proto/` onto the whole
    /// tree — which silently dropped example-monorepo's hand-authored proto sources
    /// from the index (2026-06-10).
    exclusion_layers: Vec<Gitignore>,
    /// Per-ancestor `.wqmignore` re-inclusion matchers (`!`/`- ` lines),
    /// anchored like the exclusion layers. Any match → not ignored.
    reinclusion_layers: Vec<Gitignore>,
}

impl ProjectIgnoreMatcher {
    /// Build a matcher from `.gitignore` and `.wqmignore` files.
    ///
    /// When `project_root` is `Some`, walks from `project_root` down to `dir`,
    /// accumulating ignore rules from each ancestor directory. This ensures a
    /// subdirectory scan respects patterns defined in parent directories (fixes
    /// issue #49).
    ///
    /// When `project_root` is `None`, only checks `dir` itself (legacy behaviour).
    ///
    /// Returns `None` when no ignore files exist in the search path.
    pub fn for_dir(dir: &Path, project_root: Option<&Path>) -> Option<Self> {
        let root = project_root.unwrap_or(dir);

        // Collect ancestor dirs from root down to dir (inclusive).
        let ancestors = collect_ancestor_chain(root, dir);

        let mut exclusion_layers = Vec::new();
        let mut reinclusion_layers = Vec::new();

        for ancestor in &ancestors {
            let gitignore_path = ancestor.join(".gitignore");
            let wqmignore_path = ancestor.join(".wqmignore");

            // One builder pair PER ancestor, rooted at the ancestor itself, so
            // every pattern is interpreted relative to the directory of the
            // file that declared it (gitignore semantics). `GitignoreBuilder::
            // add` anchors patterns to the BUILDER root, not the added file's
            // parent — feeding nested files into one root-level builder is
            // what broke nested negations/containment before.
            let mut exclusion_builder = GitignoreBuilder::new(ancestor);
            let mut reinc_builder = GitignoreBuilder::new(ancestor);
            let mut layer_found = false;
            let mut layer_has_reinc = false;

            if gitignore_path.exists() {
                if let Some(e) = exclusion_builder.add(&gitignore_path) {
                    warn!("Error reading {}: {}", gitignore_path.display(), e);
                }
                layer_found = true;
            }

            if wqmignore_path.exists() {
                if let Some(reinc) = parse_wqmignore_into(
                    ancestor,
                    &wqmignore_path,
                    &mut exclusion_builder,
                    &mut reinc_builder,
                ) {
                    layer_has_reinc = reinc;
                }
                layer_found = true;
            }

            if !layer_found {
                continue;
            }
            match exclusion_builder.build() {
                Ok(matcher) => exclusion_layers.push(matcher),
                Err(e) => warn!(
                    "Failed to build ignore matcher for {}: {}",
                    ancestor.display(),
                    e
                ),
            }
            if layer_has_reinc {
                match reinc_builder.build() {
                    Ok(matcher) => reinclusion_layers.push(matcher),
                    Err(e) => warn!(
                        "Failed to build re-inclusion matcher for {}: {}",
                        ancestor.display(),
                        e
                    ),
                }
            }
        }

        if exclusion_layers.is_empty() && reinclusion_layers.is_empty() {
            return None;
        }

        Some(Self {
            exclusion_layers,
            reinclusion_layers,
        })
    }

    /// Returns `true` if `path` should be excluded.
    ///
    /// Resolution: `.wqmignore` re-inclusion wins over any exclusion (allows
    /// overriding gitignore). Then the deepest layer with a decisive match
    /// wins — a nested `.gitignore` overrides its ancestors for paths under
    /// its own directory, and never applies outside it (containment).
    /// `is_dir` must be `true` when `path` refers to a directory entry.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        // Re-inclusions override: if path matches a `!`/`- ` pattern in any
        // layer that contains it, it's NOT ignored.
        for reinc in &self.reinclusion_layers {
            if !governs(reinc, path) {
                continue;
            }
            if reinc.matched(path, is_dir).is_ignore() {
                return false;
            }
        }

        // Leaf-most decisive match wins (within a layer the `ignore` crate
        // already applies gitignore's last-matching-line-wins, so per-file
        // negations like `!common/**/*.proto` resolve correctly here).
        for layer in self.exclusion_layers.iter().rev() {
            if !governs(layer, path) {
                continue;
            }
            match layer.matched(path, is_dir) {
                Match::None => continue,
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
            }
        }
        false
    }
}

/// Whether `layer` speaks for `path`: a path strictly inside the directory
/// whose ignore file the layer holds.
///
/// A directory is never matched by its OWN ignore file — git applies a
/// `.gitignore` only to what the directory contains. Matching it anyway made
/// the dequeue gate's ancestor replay test `storage/logs/` against
/// `storage/logs/.gitignore`, whose `*` the `ignore` crate matches against the
/// empty relative path: every Laravel `storage/**/.gitignore` (`*` +
/// `!.gitignore`) read as ignored at dequeue while the walk kept it, and the
/// reconciler re-enqueued the same 12 at every start (#402).
fn governs(layer: &Gitignore, path: &Path) -> bool {
    path != layer.path() && path.starts_with(layer.path())
}

/// Collect directory chain from `root` down to `dir` (inclusive).
///
/// If `dir` is not a descendant of `root`, returns just `[dir]`.
fn collect_ancestor_chain(root: &Path, dir: &Path) -> Vec<std::path::PathBuf> {
    // Spec §16 §3.1 rule 7: no fs canonicalize. Use the paths as-is —
    // both come from a single project walk and share their representation.
    let root_canon = root.to_path_buf();
    let dir_canon = dir.to_path_buf();

    if let Ok(suffix) = dir_canon.strip_prefix(&root_canon) {
        let mut chain = vec![root_canon.clone()];
        let mut current = root_canon;
        for component in suffix.components() {
            current = current.join(component);
            chain.push(current.clone());
        }
        chain
    } else {
        // dir is not under root — fall back to dir only
        vec![dir.to_path_buf()]
    }
}

/// Parse `.wqmignore`, adding exclusions to `exclusion_builder` and
/// re-inclusions to `reinc_builder`. Returns `Some(true)` if re-inclusions
/// were found, `Some(false)` if only exclusions, or `None` on read error.
fn parse_wqmignore_into(
    _dir: &Path,
    wqmignore_path: &Path,
    exclusion_builder: &mut GitignoreBuilder,
    reinc_builder: &mut GitignoreBuilder,
) -> Option<bool> {
    let content = std::fs::read_to_string(wqmignore_path).ok()?;
    let mut has_reinclusions = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Canonical `!pattern` syntax or legacy `- pattern` alias — both
        // indicate a re-inclusion that overrides .gitignore exclusions.
        let reinclusion_pattern = if let Some(p) = trimmed.strip_prefix("- ") {
            Some(p.trim())
        } else if let Some(p) = trimmed.strip_prefix('!') {
            Some(p.trim())
        } else {
            None
        };

        if let Some(pattern) = reinclusion_pattern {
            if !pattern.is_empty() {
                if let Err(e) = reinc_builder.add_line(Some(wqmignore_path.to_path_buf()), pattern)
                {
                    warn!(
                        "Malformed re-inclusion pattern '{}' in {}: {}",
                        pattern,
                        wqmignore_path.display(),
                        e
                    );
                }
                has_reinclusions = true;
            }
        } else if let Err(e) =
            exclusion_builder.add_line(Some(wqmignore_path.to_path_buf()), trimmed)
        {
            warn!(
                "Malformed exclusion pattern '{}' in {}: {}",
                trimmed,
                wqmignore_path.display(),
                e
            );
        }
    }

    Some(has_reinclusions)
}

#[cfg(test)]
mod tests;
