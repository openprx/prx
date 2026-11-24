//! Process groups of a worker's descendants that live outside the worker's own group.
//!
//! The shell tool starts every command in its own process group, so a `killpg`
//! of a session worker's group never reaches those commands: they are
//! reparented to init and keep running. The killer takes a snapshot of the
//! worker's process tree before it signals the worker (afterwards the parent
//! links are gone) and signals every group found in it. Nothing here depends on
//! the worker running code after `SIGKILL`.

use std::collections::{HashMap, HashSet};

/// Group ids of all live descendants of `root_pid` that are not in `root_pgid`
/// and are neither init nor this process's own group.
#[cfg(unix)]
pub(crate) fn foreign_groups_of(root_pid: i32, root_pgid: i32) -> Vec<i32> {
    // SAFETY: `getpgrp` takes no arguments, cannot fail and touches no memory.
    #[allow(unsafe_code)]
    let own_pgid = unsafe { libc::getpgrp() };
    let table = process_table();
    let mut seen: HashSet<i32> = HashSet::from([root_pid]);
    let mut frontier = vec![root_pid];
    let mut groups: Vec<i32> = Vec::new();
    while let Some(parent) = frontier.pop() {
        for (pid, (ppid, pgid)) in &table {
            if *ppid == parent && seen.insert(*pid) {
                frontier.push(*pid);
                if *pgid > 1 && *pgid != root_pgid && *pgid != own_pgid && !groups.contains(pgid) {
                    groups.push(*pgid);
                }
            }
        }
    }
    groups
}

/// `SIGKILL` each group; errors (group already gone) are ignored.
#[cfg(unix)]
#[allow(unsafe_code)]
pub(crate) fn kill_groups(groups: &[i32]) {
    for pgid in groups.iter().copied().filter(|pgid| *pgid > 1) {
        // SAFETY: `pgid` is a positive group id read from the process table;
        // `killpg` only delivers a signal and dereferences no pointer.
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
}

/// pid -> (ppid, pgid) for every visible process.
#[cfg(target_os = "linux")]
fn process_table() -> HashMap<i32, (i32, i32)> {
    let mut table = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return table;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The command name is parenthesised and may contain spaces or parens;
        // the fields after the last `)` are: state ppid pgrp ...
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace().skip(1);
        let ppid = fields.next().and_then(|field| field.parse::<i32>().ok());
        let pgid = fields.next().and_then(|field| field.parse::<i32>().ok());
        if let (Some(ppid), Some(pgid)) = (ppid, pgid) {
            table.insert(pid, (ppid, pgid));
        }
    }
    table
}

/// pid -> (ppid, pgid) for every visible process, read through `ps`.
#[cfg(all(unix, not(target_os = "linux")))]
fn process_table() -> HashMap<i32, (i32, i32)> {
    let mut table = HashMap::new();
    let Ok(output) = std::process::Command::new("ps")
        .args(["-ax", "-o", "pid=,ppid=,pgid="])
        .output()
    else {
        return table;
    };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace().map(|field| field.parse::<i32>().ok());
        if let (Some(Some(pid)), Some(Some(ppid)), Some(Some(pgid))) = (fields.next(), fields.next(), fields.next()) {
            table.insert(pid, (ppid, pgid));
        }
    }
    table
}
