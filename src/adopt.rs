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

use serde_json::json;

use crate::api::ApiClient;
use crate::config::load_config;
use crate::mirror::{
    export_layout_root, fetch_snapshot, locate_in_layout, pane_is_mirror, PaneInfo,
};
use crate::remote::RemoteHost;
use crate::state::load_state;
use crate::util::{Env, Result};

const SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

// NOTE: workspace adoption (new-space-from-a-mirror → remote workspace) was
// attempted 2026-07-07 and REVERTED after a runaway: each mirrored-back
// workspace is itself briefly an unmapped single-pane sentinel workspace, so
// concurrent adopt invocations treated the daemon's own mirror-backs as
// strays and created remote workspaces in a feedback loop (~245 junk
// workspaces). A safe version needs, at minimum: an exclusive lock across
// adopt invocations, a daemon-ownership marker readable BEFORE the state
// file is saved (e.g. label prefix set at creation), and close-stray-first
// ordering so a second pass can never re-adopt the same workspace.

pub async fn run(env: Env) -> Result<()> {
    tokio::time::sleep(SETTLE).await;
    let config = load_config(&env.config_dir)?;
    let local = ApiClient::connect(&env.local_socket).await?;
    let snap = fetch_snapshot(&local).await?;

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
