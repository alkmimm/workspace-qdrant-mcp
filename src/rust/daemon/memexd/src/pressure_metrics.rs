//! Resource-pressure gauges the daemon samples about ITSELF.
//!
//! Every series here exists because its absence hid a failure in 2026-09:
//! `lsp_active_servers` said 1 while ~50 tsserver processes sat under the
//! daemon (8 GB/h), and `unified_queue_depth{status="failed"}` was simply
//! absent while 1,571 items sat failed — an absent series reads as "fine" on
//! every panel. The rule this module enforces: report what the OS and the
//! queue actually hold, and make the "nothing wrong" value a real 0, never a
//! missing sample. Dashboard: docker/grafana/dashboards/resource-pressure.json.

use std::collections::{HashMap, HashSet};

use workspace_qdrant_core::monitoring::METRICS;

/// Walk /proc once and count the processes rooted at `root_pid`: direct
/// children, deeper descendants, and their summed resident memory.
///
/// One pass reads every `/proc/<pid>/stat` for (ppid, rss) and builds the
/// parent map in memory, then follows it down from the root — so the cost is
/// one file per process on the host, not one per level. Zombies count: a
/// zombie under the daemon is still a reaping failure worth seeing.
pub(crate) fn sample_process_tree(root_pid: u32) -> (usize, usize, u64) {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return (0, 0, 0);
    };
    // pid -> (ppid, rss_bytes)
    let mut procs: HashMap<u32, (u32, u64)> = HashMap::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        if let Some((ppid, rss)) = parse_proc_stat_ppid_rss(&stat) {
            procs.insert(pid, (ppid, rss));
        }
    }

    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, &(ppid, _)) in &procs {
        children_of.entry(ppid).or_default().push(pid);
    }

    let direct = children_of.get(&root_pid).cloned().unwrap_or_default();
    let mut descendants = 0usize;
    let mut rss_total = 0u64;
    let mut stack: Vec<(u32, bool)> = direct.iter().map(|&p| (p, true)).collect();
    while let Some((pid, is_direct)) = stack.pop() {
        if !is_direct {
            descendants += 1;
        }
        rss_total += procs.get(&pid).map(|&(_, r)| r).unwrap_or(0);
        if let Some(kids) = children_of.get(&pid) {
            stack.extend(kids.iter().map(|&k| (k, false)));
        }
    }
    (direct.len(), descendants, rss_total)
}

/// `(ppid, rss_bytes)` from a `/proc/<pid>/stat` line.
///
/// The comm field (2) is parenthesised and may itself contain spaces and
/// parentheses, so fields are located from the LAST `)` rather than by
/// splitting on whitespace from the start. After it: state(3) ppid(4) ... and
/// rss is field 24, i.e. index 21 counting from `state` as 0.
fn parse_proc_stat_ppid_rss(stat: &str) -> Option<(u32, u64)> {
    let after = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = after.split_whitespace().collect();
    let ppid = fields.get(1)?.parse().ok()?;
    let rss_pages: u64 = fields.get(21)?.parse().ok()?;
    Some((ppid, rss_pages * 4096))
}

/// The queue statuses a depth panel must always be able to read.
const QUEUE_STATUSES: [&str; 3] = ["pending", "in_progress", "failed"];

/// Give every item_type present in `seen` all three statuses, writing 0 for
/// the ones the GROUP BY did not return, and record them in `seen` so the
/// exporter's zero-out pass keeps them alive.
///
/// The aggregate only yields rows that exist, so `status="failed"` used to
/// appear the first time something failed and never before — a fresh daemon
/// with an empty failed set exported no series, which every consumer read as
/// "no failures" and a `> N` alert could never distinguish from "no metric".
pub(crate) fn fill_missing_queue_statuses(seen: &mut HashSet<(String, String)>) {
    let item_types: Vec<String> = seen
        .iter()
        .map(|(item_type, _)| item_type.clone())
        .collect();
    for item_type in item_types {
        for status in QUEUE_STATUSES {
            let pair = (item_type.clone(), status.to_string());
            if !seen.contains(&pair) {
                METRICS.set_unified_queue_depth(&pair.0, &pair.1, 0);
                seen.insert(pair);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ppid and rss located from the LAST `)`, so a comm like
    /// `(tsserver (partial) js)` cannot shift the fields.
    #[test]
    fn ppid_and_rss_from_stat_with_hostile_comm() {
        // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt
        // majflt cmajflt utime stime cutime cstime priority nice threads
        // itrealvalue starttime vsize rss ...
        let stat = "4242 (tsserver (partial) js) S 5631 5631 5631 0 -1 4194304 \
                    100 0 0 0 7 3 0 0 20 0 11 0 12345 987654321 17000 ...";
        assert_eq!(parse_proc_stat_ppid_rss(stat), Some((5631, 17000 * 4096)));
        assert_eq!(parse_proc_stat_ppid_rss("garbage"), None);
        assert_eq!(parse_proc_stat_ppid_rss("1 (x) S"), None);
    }

    /// The leak this measures: language servers fork workers the daemon never
    /// sees. A real forking child proves the walk follows the tree down, and
    /// that a grandchild counts as `descendant`, not `child`.
    #[cfg(unix)]
    #[test]
    fn process_tree_counts_children_and_grandchildren() {
        use std::io::Read;
        // sh forks two sleepers, prints "ready", then waits — like a language
        // server holding its workers.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & sleep 30 & echo ready; wait"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut buf = [0u8; 6];
        child.stdout.as_mut().unwrap().read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ready\n");

        let (children, descendants, rss) = sample_process_tree(std::process::id());
        // Other tests in this binary may have spawned processes concurrently,
        // so the bounds are floors, not equalities.
        assert!(
            children >= 1,
            "the sh must be a direct child, got {children}"
        );
        assert!(
            descendants >= 2,
            "the two sleepers must be descendants, got {descendants}"
        );
        assert!(rss > 0, "three live processes have nonzero RSS");

        let _ = child.kill();
        let _ = child.wait();
    }

    /// A queue that has only ever held pending items must still export
    /// `failed` = 0 for that item_type, and the pair must land in `seen` so
    /// the exporter keeps zeroing it on later ticks.
    #[test]
    fn failed_status_is_exported_as_zero_when_absent() {
        // Unique item_type: METRICS is process-global and other tests write it.
        let item_type = format!("fill_test_{}", std::process::id());
        let mut seen: HashSet<(String, String)> = HashSet::new();
        seen.insert((item_type.clone(), "pending".to_string()));

        fill_missing_queue_statuses(&mut seen);

        for status in QUEUE_STATUSES {
            assert!(
                seen.contains(&(item_type.clone(), status.to_string())),
                "{status} missing"
            );
        }
        assert_eq!(
            METRICS
                .unified_queue_depth
                .with_label_values(&[&item_type, "failed"])
                .get(),
            0
        );
        assert_eq!(seen.len(), 3);
    }
}
