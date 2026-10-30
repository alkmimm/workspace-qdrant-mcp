//! The watcher's directory set: what it skips, that events still arrive from
//! what it keeps, and that it follows directories created, renamed or
//! re-included after it started. The live tests drive a real inotify watcher
//! over a tempdir.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use notify::event::{
    AccessKind, AccessMode, CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode,
};
use notify::{Event, EventKind, RecommendedWatcher};

use super::*;

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn rel_set(root: &Path, dirs: &[PathBuf]) -> BTreeSet<String> {
    dirs.iter()
        .map(|d| {
            d.strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

/// A project tree holding every kind of directory the watcher must skip.
fn tree() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let r = dir.path();
    write(r, ".gitignore", "node_modules/\nbuild/\n");
    write(r, ".wqmignore", "/vendor/\n");
    write(r, "src/a/lib.rs", "fn a() {}");
    write(r, "src/node_modules/pkg/index.js", "");
    write(r, "node_modules/pkg/index.js", "");
    write(r, "build/out/app.rs", "");
    write(r, "vendor/lib/x.c", "");
    write(r, "generated/g.rs", "");
    std::fs::create_dir_all(r.join(".git/objects/aa")).unwrap();
    write(
        r,
        ".claude/worktrees/wt1/.git",
        "gitdir: /repo/.git/worktrees/wt1\n",
    );
    write(r, ".claude/worktrees/wt1/src/b.rs", "fn b() {}");
    write(r, "sub/.git", "gitdir: ../.git/modules/sub\n");
    write(r, "sub/s.rs", "fn s() {}");
    dir
}

type Events = mpsc::Receiver<notify::Result<Event>>;

fn live(root: &Path) -> (RecommendedWatcher, Events, DirWatches) {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .unwrap();
    let mut watches = DirWatches::new(root.to_path_buf(), None);
    watches.sync(&mut watcher).unwrap();
    (watcher, rx, watches)
}

/// Every path events name within `wait`, applying the directory changes they
/// imply the way the watcher's follower does. Returns early once `until` is seen.
fn pump(
    rx: &Events,
    watcher: &mut RecommendedWatcher,
    watches: &mut DirWatches,
    until: Option<&Path>,
    wait: Duration,
) -> Vec<PathBuf> {
    let deadline = Instant::now() + wait;
    let mut seen = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(result) = rx.recv_timeout(left) else {
            break;
        };
        let Ok(event) = result else { continue };
        for change in dir_changes(&event) {
            watches.apply(watcher, &change).unwrap();
        }
        let hit = until.is_some_and(|p| event.paths.iter().any(|e| e == p));
        seen.extend(event.paths);
        if hit {
            break;
        }
    }
    seen
}

#[test]
fn ignored_dirs_dot_git_and_linked_worktrees_are_not_watched() {
    let dir = tree();
    let r = dir.path();
    let gdir = tempfile::tempdir().unwrap();
    let global = gdir.path().join("global.wqmignore");
    std::fs::write(&global, "generated/\n").unwrap();

    let expected: BTreeSet<String> = ["", ".claude", ".claude/worktrees", "src", "src/a", "sub"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(rel_set(r, &watchable_dirs(r, r, Some(&global))), expected);
    assert!(
        watchable_dirs(r, &r.join("node_modules"), Some(&global)).is_empty(),
        "an ignored subtree reached directly is skipped too"
    );
}

#[test]
fn events_arrive_from_kept_dirs_and_never_from_ignored_ones() {
    let dir = tree();
    let r = dir.path();
    let (mut watcher, rx, mut watches) = live(r);

    write(r, "node_modules/pkg/index.js", "changed");
    write(r, ".claude/worktrees/wt1/src/b.rs", "changed");
    let kept = r.join("src/a/lib.rs");
    write(r, "src/a/lib.rs", "changed");

    let seen = pump(
        &rx,
        &mut watcher,
        &mut watches,
        Some(&kept),
        Duration::from_secs(10),
    );
    assert!(
        seen.contains(&kept),
        "kept file never raised an event: {seen:?}"
    );
    let leaked: Vec<_> = seen
        .iter()
        .filter(|p| {
            p.starts_with(r.join("node_modules")) || p.starts_with(r.join(".claude/worktrees/wt1"))
        })
        .collect();
    assert!(leaked.is_empty(), "events from unwatched dirs: {leaked:?}");
}

#[test]
fn directories_created_or_renamed_later_are_followed() {
    let dir = tree();
    let r = dir.path();
    let (mut watcher, rx, mut watches) = live(r);

    std::fs::create_dir_all(r.join("src/new/deep")).unwrap();
    pump(
        &rx,
        &mut watcher,
        &mut watches,
        None,
        Duration::from_millis(500),
    );
    let created = r.join("src/new/deep/f.rs");
    write(r, "src/new/deep/f.rs", "fn f() {}");
    let seen = pump(
        &rx,
        &mut watcher,
        &mut watches,
        Some(&created),
        Duration::from_secs(10),
    );
    assert!(seen.contains(&created), "file in a new directory: {seen:?}");

    std::fs::create_dir_all(r.join("src/new/node_modules/x")).unwrap();
    pump(
        &rx,
        &mut watcher,
        &mut watches,
        None,
        Duration::from_millis(500),
    );
    assert!(
        !watches.is_watched(&r.join("src/new/node_modules")),
        "ignored once created too"
    );

    std::fs::rename(r.join("src/new"), r.join("src/renamed")).unwrap();
    pump(
        &rx,
        &mut watcher,
        &mut watches,
        None,
        Duration::from_millis(500),
    );
    let moved = r.join("src/renamed/deep/g.rs");
    write(r, "src/renamed/deep/g.rs", "fn g() {}");
    let seen = pump(
        &rx,
        &mut watcher,
        &mut watches,
        Some(&moved),
        Duration::from_secs(10),
    );
    assert!(
        seen.contains(&moved),
        "file in a renamed directory, under its new name: {seen:?}"
    );
    assert!(
        !watches.is_watched(&r.join("src/new")),
        "the old name is forgotten"
    );
}

#[test]
fn an_ignore_rule_change_resyncs_the_watched_set() {
    let dir = tree();
    let r = dir.path();
    let (mut watcher, _rx, mut watches) = live(r);
    assert!(watches.is_watched(&r.join("src/a")));

    write(r, ".gitignore", "node_modules/\nbuild/\nsrc/a/\n");
    watches
        .apply(&mut watcher, &DirChange::IgnoreRulesChanged)
        .unwrap();
    assert!(
        !watches.is_watched(&r.join("src/a")),
        "newly ignored: unwatched"
    );

    write(r, ".gitignore", "build/\n");
    watches
        .apply(&mut watcher, &DirChange::IgnoreRulesChanged)
        .unwrap();
    assert!(
        watches.is_watched(&r.join("src/a")),
        "re-included: watched again"
    );
    assert!(
        watches.is_watched(&r.join("node_modules/pkg")),
        "no longer ignored"
    );
}

/// The resync walk reads every ignore file and inotify reports each read. A read
/// must not count as a rule change, or every resync triggers the next one.
#[test]
fn reading_the_ignore_files_does_not_trigger_another_resync() {
    let dir = tree();
    let r = dir.path();
    let (mut watcher, rx, mut watches) = live(r);

    watches
        .apply(&mut watcher, &DirChange::IgnoreRulesChanged)
        .unwrap();
    let events: Vec<Event> =
        std::iter::from_fn(|| rx.recv_timeout(Duration::from_millis(500)).ok())
            .filter_map(Result::ok)
            .collect();
    let gitignore = r.join(".gitignore");
    assert!(
        events.iter().any(|e| e.paths.contains(&gitignore)),
        "the walk's read of .gitignore is observed: {events:?}"
    );
    assert!(
        events
            .iter()
            .all(|e| !dir_changes(e).contains(&DirChange::IgnoreRulesChanged)),
        "a read is not a rule change: {events:?}"
    );
}

#[test]
fn dir_changes_reads_directory_and_ignore_file_events() {
    let dir = tempfile::tempdir().unwrap();
    let existing = dir.path().join("to");
    std::fs::create_dir_all(&existing).unwrap();
    let ev = |kind, paths: &[&Path]| {
        paths
            .iter()
            .fold(Event::new(kind), |e, p| e.add_path(p.to_path_buf()))
    };
    let a = Path::new("/r/a");

    assert_eq!(
        dir_changes(&ev(EventKind::Create(CreateKind::Folder), &[a])),
        vec![DirChange::Added(a.into())]
    );
    assert_eq!(
        dir_changes(&ev(EventKind::Remove(RemoveKind::Folder), &[a])),
        vec![DirChange::Removed(a.into())]
    );
    assert!(dir_changes(&ev(
        EventKind::Create(CreateKind::File),
        &[Path::new("/r/a.rs")]
    ))
    .is_empty());
    assert_eq!(
        dir_changes(&ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[a, &existing]
        )),
        vec![
            DirChange::Removed(a.into()),
            DirChange::Added(existing.clone())
        ]
    );
    assert_eq!(
        dir_changes(&ev(
            EventKind::Modify(ModifyKind::Data(DataChange::Content)),
            &[Path::new("/r/.gitignore")]
        )),
        vec![DirChange::IgnoreRulesChanged]
    );
    assert!(dir_changes(&ev(
        EventKind::Access(AccessKind::Open(AccessMode::Any)),
        &[Path::new("/r/.gitignore")]
    ))
    .is_empty());
}
