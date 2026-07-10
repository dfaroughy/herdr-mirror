// Auto-adopt: make every pane-creation path machine-aware. Fired by the
// pane.created plugin event: a native split inside a mirror workspace (right-click
// pane menu, prefix+v / prefix+minus — herdr core knows nothing about machines)
// creates a LOCAL shell pane; this pass replaces it with the equivalent REMOTE
// split, which the daemon then mirrors back.
//
// A stray is recognized by conjunction: it sits in a mirror workspace, its cwd
// is the wrapper sentinel dir (.mirror-pane — native splits inherit it from the
// focused wrapper pane), and the daemon's state map does NOT own it. Wrapper
// panes the daemon created but hasn't saved yet are the one race — SETTLE
// outwaits a converge save so nascent wrappers are owned by scan time.
//
// If the remote host is unreachable the stray is left alone: a working local
// pane beats an error toast.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::AsRawFd;

use serde_json::json;

use crate::api::ApiClient;
use crate::config::{load_config, MirrorConfig};
use crate::mirror::{
    export_layout_root, fetch_snapshot, locate_in_layout, mirror_cwd_host, pane_is_mirror,
    PaneInfo, Snapshot,
};
use crate::remote::RemoteHost;
use crate::state::load_state;
use crate::util::{Env, Result};

const SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

/// One adopt at a time, machine-wide. pane.created fires once per new pane, so
/// bursts (daemon rebuilds, multi-pane restores) spawn concurrent invocations;
/// unserialized, they double-create remote objects — the 2026-07-07 runaway.
/// Held for the whole run; blocking is fine (invocations are short-lived).
fn adopt_lock(env: &Env) -> Result<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(env.state_dir.join("adopt.lock"))?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(crate::util::err("adopt: flock failed"));
    }
    Ok(f)
}

/// Workspace adoption, take two (the 2026-07-07 runaway is documented in the
/// repo history at 9044fcf). Its three lessons, applied:
///   1. the flock above — concurrent invocations serialize;
///   2. daemon mirror-back workspaces are recognizable from BIRTH: converge
///      creates them with their "<prefix>: <name>" label in workspace.create
///      itself, so the label guard has no unmapped window;
///   3. close-the-stray-FIRST: workspace.close succeeds exactly once, so the
///      instance that wins the close is the only one that creates remotely.
/// Host resolution: wrapper panes now live in .mirror-pane/<host>, and a
/// workspace born from a focused mirror pane inherits that cwd — the stray
/// itself says which host the user meant. Legacy flat cwds fall back to
/// default_host.
async fn adopt_workspaces(env: &Env, config: &MirrorConfig, local: &ApiClient, snap: &Snapshot) {
    let mapped: BTreeSet<String> = config
        .hosts
        .iter()
        .flat_map(|h| {
            load_state(&env.state_dir, &h.name)
                .workspaces
                .values()
                .filter(|e| !e.is_tombstoned())
                .map(|e| e.local_id.clone())
                .collect::<Vec<_>>()
        })
        .collect();

    for ws in &snap.workspaces {
        if mapped.contains(&ws.workspace_id) {
            continue;
        }
        // daemon-owned mirror-backs carry their host prefix from birth
        if config.hosts.iter().any(|h| ws.label.starts_with(&format!("{}: ", h.prefix))) {
            continue;
        }
        let panes: Vec<&PaneInfo> =
            snap.panes.iter().filter(|p| p.workspace_id == ws.workspace_id).collect();
        // strays are fresh single-pane sentinel-cwd workspaces; anything the
        // user has built in stays untouched
        if panes.len() != 1 || !pane_is_mirror(panes[0]) {
            continue;
        }
        let host = mirror_cwd_host(panes[0])
            .and_then(|name| config.hosts.iter().find(|h| h.name == name))
            .or_else(|| config.default_host());
        let Some(host) = host else { continue };

        // claim by closing FIRST: an empty seconds-old shell, nothing to lose,
        // and only one instance can win this close
        if local
            .request("workspace.close", json!({ "workspace_id": ws.workspace_id }))
            .await
            .is_err()
        {
            continue; // another instance (or the user) got there first
        }
        let mut remote = RemoteHost::new(host, &env.state_dir);
        match remote.connect_api().await {
            Ok((api, _)) => {
                match api.request("workspace.create", json!({ "focus": false })).await {
                    Ok(_) => println!(
                        "adopt: replaced local workspace {} with a new remote workspace on {}",
                        ws.workspace_id, host.name
                    ),
                    Err(e) => println!(
                        "adopt: {} workspace.create failed ({e}) — local stray already closed",
                        host.name
                    ),
                }
            }
            Err(e) => println!(
                "adopt: {} unreachable ({e}) — local stray closed, no remote workspace created",
                host.name
            ),
        }
    }
}

pub async fn run(env: Env) -> Result<()> {
    tokio::time::sleep(SETTLE).await;
    let _lock = adopt_lock(&env)?;
    let config = load_config(&env.config_dir)?;
    let local = ApiClient::connect(&env.local_socket).await?;
    let snap = fetch_snapshot(&local).await?;

    adopt_workspaces(&env, &config, &local, &snap).await;

    for host in &config.hosts {
        let state = load_state(&env.state_dir, &host.name);
        let ws_local: BTreeSet<&String> = state
            .workspaces
            .values()
            .filter(|e| !e.is_tombstoned())
            .map(|e| &e.local_id)
            .collect();
        let owned: BTreeSet<&String> = state
            .panes
            .values()
            .filter(|e| !e.is_tombstoned())
            .map(|e| &e.local_id)
            .collect();
        let pane_rid: BTreeMap<&String, &String> = state
            .panes
            .iter()
            .filter(|(_, e)| !e.is_tombstoned())
            .map(|(rid, e)| (&e.local_id, rid))
            .collect();

        let strays: Vec<&PaneInfo> = snap
            .panes
            .iter()
            .filter(|p| {
                ws_local.contains(&p.workspace_id)
                    && !owned.contains(&p.pane_id)
                    && pane_is_mirror(p)
            })
            .collect();
        if strays.is_empty() {
            continue;
        }

        // resolve every stray's (source, direction) from the local split tree
        // BEFORE closing anything — the tree is exact (it records how the user
        // actually split), and closing the stray reshapes it
        let mut resolved: Vec<(&PaneInfo, String, String)> = Vec::new();
        for stray in strays {
            let placed = match export_layout_root(&local, &stray.tab_id).await {
                Some(root) => locate_in_layout(&root, &stray.pane_id),
                None => None,
            };
            let src_rid = placed.as_ref().and_then(|(_, sibs)| {
                sibs.iter().find(|id| owned.contains(id)).and_then(|id| pane_rid.get(id))
            });
            match (placed.as_ref(), src_rid) {
                (Some((dir, _)), Some(rid)) => {
                    resolved.push((stray, dir.clone(), (*rid).clone()));
                }
                _ => println!(
                    "adopt: stray {} does not resolve to a mirror sibling — leaving it local",
                    stray.pane_id
                ),
            }
        }
        if resolved.is_empty() {
            continue;
        }

        // the stray is a seconds-old empty shell: closing it early loses
        // nothing and ends the "phantom local pane" flash before the slow
        // part (ssh round-trip) begins
        for (stray, _, _) in &resolved {
            let _ = local.request("pane.close", json!({ "pane_id": stray.pane_id })).await;
        }

        let mut remote = RemoteHost::new(host, &env.state_dir);
        let (api, _status) = match remote.connect_api().await {
            Ok(c) => c,
            Err(e) => {
                println!(
                    "adopt: {} unreachable ({e}) — local split(s) closed, no remote pane created",
                    host.name
                );
                continue;
            }
        };
        let rsnap = fetch_snapshot(&api).await?;

        for (stray, dir, rid) in &resolved {
            let cwd = rsnap
                .panes
                .iter()
                .find(|p| &p.pane_id == rid)
                .and_then(|p| p.foreground_cwd.clone().or_else(|| p.cwd.clone()));
            api.request(
                "pane.split",
                json!({ "target_pane_id": rid, "direction": dir, "cwd": cwd, "focus": false }),
            )
            .await?;
            println!(
                "adopt: replaced local pane {} with a remote {dir} split of {rid} on {}",
                stray.pane_id, host.name
            );
        }
    }
    Ok(())
}
