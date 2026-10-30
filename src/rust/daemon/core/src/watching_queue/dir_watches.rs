//! Which directories the file watcher registers: one non-recursive watch for
//! each directory the project walk keeps.
//!
//! A recursive inotify watch adds one watch per directory under the root,
//! ignored ones included. Measured 2026-10-30: memexd held 348,580 watches —
//! exactly the directory count of the ten roots it managed to watch (in Finance
//! 33k of 35k directories were `node_modules`/`build`/`.dart_tool`) — while
//! tecsul, 400k directories with 388k of them in agent worktrees under
//! `.claude/`, could never start its watcher ("OS file watch limit reached",
//! retried every ~8 minutes) and `wsl-maint`, started next, failed with it.
//! Events from an ignored directory are dropped anyway, so watching it only
//! spends the per-user inotify budget.
//!
//! The pruning walk honours exactly the ignore gate's sources (the project
//! walker plus `global.wqmignore`, as the reconciler's eligible walk does), and
//! also skips `.git` and linked worktree checkouts: the worktree discovery
//! follows those by git state, and their files are not this tree's content.
//! Directories created or renamed inside the tree are followed through
//! [`dir_changes`]; an ignore-file change re-syncs the whole set.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecursiveMode, Watcher};

use crate::patterns::ignore_gate::IgnoreGate;
use crate::patterns::project_walk::project_walk_builder;

/// A change to the set of directories worth watching, read off a notify event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DirChange {
    /// A directory appeared (created, or renamed into place).
    Added(PathBuf),
    /// A directory went away (removed, or renamed out of place).
    Removed(PathBuf),
    /// A `.gitignore` / `.wqmignore` changed: what is ignored may differ now.
    IgnoreRulesChanged,
}

/// The directory changes one notify event implies. Runs on the notify thread,
/// so it only classifies (and stats a path whose kind the event leaves open).
pub(super) fn dir_changes(event: &Event) -> Vec<DirChange> {
    let mut changes = Vec::new();
    if can_change_content(&event.kind) && event.paths.iter().any(|p| is_ignore_file(p)) {
        changes.push(DirChange::IgnoreRulesChanged);
    }
    let added = |p: &PathBuf| DirChange::Added(p.clone());
    let removed = |p: &PathBuf| DirChange::Removed(p.clone());
    match event.kind {
        EventKind::Create(CreateKind::Folder) => changes.extend(event.paths.iter().map(added)),
        EventKind::Create(CreateKind::Any)
        | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            changes.extend(event.paths.iter().filter(|p| p.is_dir()).map(added))
        }
        EventKind::Remove(RemoveKind::Folder)
        | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            changes.extend(event.paths.iter().map(removed))
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
            if let [from, to] = event.paths.as_slice() {
                changes.push(removed(from));
                if to.is_dir() {
                    changes.push(added(to));
                }
            }
        }
        _ => {}
    }
    changes
}

/// Whether an event can change a file's bytes or existence. Reads (`Access`) and
/// metadata-only changes cannot. The content pipeline drops the rest, and an
/// ignore file counts as changed only on these: inotify reports every open, and
/// the resync walk itself reads each ignore file, so counting reads made every
/// resync trigger the next.
pub(super) fn can_change_content(kind: &EventKind) -> bool {
    !matches!(
        kind,
        EventKind::Access(_) | EventKind::Modify(ModifyKind::Metadata(_))
    )
}

fn is_ignore_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n == ".gitignore" || n == ".wqmignore")
}

/// The directories of `root`'s tree at and under `start` worth watching:
/// everything the project walk keeps, minus `.git` and linked worktree
/// checkouts. `start` itself is returned unless it is ignored (the root never
/// is). `global` is the resolved `global.wqmignore`, if any.
pub(super) fn watchable_dirs(root: &Path, start: &Path, global: Option<&Path>) -> Vec<PathBuf> {
    let gate = Arc::new(IgnoreGate::for_dir(start, Some(root), global));
    if start != root && (gate.is_dir_ignored_with_ancestors(root, start) || skipped_dir(start)) {
        return Vec::new();
    }
    let mut builder = project_walk_builder(start);
    builder.add_custom_ignore_filename(".wqmignore");
    let filter_gate = Arc::clone(&gate);
    builder.filter_entry(move |entry| {
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        is_dir
            && (entry.depth() == 0
                || !(skipped_dir(entry.path()) || filter_gate.is_ignored(entry.path(), true)))
    });
    builder
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_dir()))
        .map(|entry| entry.into_path())
        .collect()
}

/// `.git`, and linked worktree checkouts (a `.git` gitlink FILE pointing into
/// `<repo>/.git/worktrees/<name>`). A submodule's gitlink points into
/// `.git/modules/` and stays watched: its files are this tree's content.
fn skipped_dir(dir: &Path) -> bool {
    if dir.file_name().is_some_and(|n| n == ".git") {
        return true;
    }
    let Ok(gitlink) = std::fs::read_to_string(dir.join(".git")) else {
        return false; // no `.git`, or a `.git` directory (a nested repository)
    };
    gitlink
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("gitdir:"))
        .is_some_and(|target| target.trim().replace('\\', "/").contains("/worktrees/"))
}

/// The directories one watcher has registered, each non-recursively.
pub(super) struct DirWatches {
    root: PathBuf,
    global: Option<PathBuf>,
    watched: HashSet<PathBuf>,
}

impl DirWatches {
    pub(super) fn new(root: PathBuf, global: Option<PathBuf>) -> Self {
        Self {
            root,
            global,
            watched: HashSet::new(),
        }
    }

    /// How many directories are watched.
    pub(super) fn len(&self) -> usize {
        self.watched.len()
    }

    /// Forget every directory (the watcher holding them was dropped).
    pub(super) fn clear(&mut self) {
        self.watched.clear();
    }

    #[cfg(test)]
    fn is_watched(&self, dir: &Path) -> bool {
        self.watched.contains(dir)
    }

    /// Watch every directory the walk keeps, and stop watching the ones it no
    /// longer keeps. Stops at the first directory the watcher refuses.
    pub(super) fn sync(&mut self, watcher: &mut dyn Watcher) -> notify::Result<()> {
        let wanted: HashSet<PathBuf> =
            watchable_dirs(&self.root, &self.root, self.global.as_deref())
                .into_iter()
                .collect();
        let stale: Vec<PathBuf> = self.watched.difference(&wanted).cloned().collect();
        for dir in stale {
            let _ = watcher.unwatch(&dir);
            self.watched.remove(&dir);
        }
        for dir in wanted {
            self.watch(watcher, dir)?;
        }
        Ok(())
    }

    /// A directory that appeared inside the tree: watch it and what it holds.
    pub(super) fn add_subtree(
        &mut self,
        watcher: &mut dyn Watcher,
        dir: &Path,
    ) -> notify::Result<()> {
        if !dir.starts_with(&self.root) || !dir.is_dir() {
            return Ok(());
        }
        for sub in watchable_dirs(&self.root, dir, self.global.as_deref()) {
            self.watch(watcher, sub)?;
        }
        Ok(())
    }

    /// A directory that went away: forget it and everything watched under it.
    /// Only a directory this set watches can have watched descendants.
    pub(super) fn remove_subtree(&mut self, watcher: &mut dyn Watcher, dir: &Path) {
        if !self.watched.contains(dir) {
            return;
        }
        let gone: Vec<PathBuf> = self
            .watched
            .iter()
            .filter(|p| p.starts_with(dir))
            .cloned()
            .collect();
        for path in gone {
            // A removed directory's watch is already gone in the kernel.
            let _ = watcher.unwatch(&path);
            self.watched.remove(&path);
        }
    }

    /// Apply one [`DirChange`].
    pub(super) fn apply(
        &mut self,
        watcher: &mut dyn Watcher,
        change: &DirChange,
    ) -> notify::Result<()> {
        match change {
            DirChange::Added(dir) => self.add_subtree(watcher, dir),
            DirChange::Removed(dir) => {
                self.remove_subtree(watcher, dir);
                Ok(())
            }
            DirChange::IgnoreRulesChanged => self.sync(watcher),
        }
    }

    fn watch(&mut self, watcher: &mut dyn Watcher, dir: PathBuf) -> notify::Result<()> {
        if self.watched.contains(&dir) {
            return Ok(());
        }
        watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        self.watched.insert(dir);
        Ok(())
    }
}

#[cfg(test)]
#[path = "dir_watches_tests.rs"]
mod tests;
