// Auto-adopt: make every pane-creation path machine-aware. Fired by the
// pane.split plugin event: a native split inside a mirror workspace (right-click
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
use crate::mirror::{fetch_snapshot, pane_is_mirror, PaneInfo, Snapshot};
use crate::remote::RemoteHost;
use crate::state::load_state;
use crate::util::{Env, Result};

const SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

fn rect(snap: &Snapshot, tab_id: &str, pane_id: &str) -> Option<(u32, u32)> {
    snap.layouts
        .iter()
        .find(|l| l.tab_id == tab_id)?
        .panes
        .iter()
        .find(|p| p.pane_id == pane_id)
        .map(|p| (p.rect.width, p.rect.height))
}

/// Which owned sibling was this stray split off from, and in which direction?
/// A split preserves the perpendicular dimension: equal widths → the tiles are
/// stacked (down), equal heights → side by side (right). Without layout data,
/// fall back to the first sibling and "right" — a slightly wrong direction on
/// the correct machine still beats a pane on the wrong machine.
fn pick_source<'a>(
    snap: &Snapshot,
    stray: &PaneInfo,
    sibs: &[&'a PaneInfo],
) -> Option<(&'a PaneInfo, &'static str)> {
    if let Some((sw, sh)) = rect(snap, &stray.tab_id, &stray.pane_id) {
        let mut by_height: Option<&PaneInfo> = None;
        for s in sibs {
            let Some((w, h)) = rect(snap, &s.tab_id, &s.pane_id) else { continue };
            if w == sw {
                return Some((s, "down"));
            }
            if h == sh && by_height.is_none() {
                by_height = Some(s);
            }
        }
        if let Some(s) = by_height {
            return Some((s, "right"));
        }
    }
    sibs.first().map(|s| (*s, "right"))
}

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

        let mut remote = RemoteHost::new(host, &env.state_dir);
        let (api, _status) = match remote.connect_api().await {
            Ok(c) => c,
            Err(e) => {
                println!("adopt: {} unreachable ({e}) — leaving local pane(s) as-is", host.name);
                continue;
            }
        };
        let rsnap = fetch_snapshot(&api).await?;

        for stray in strays {
            let sibs: Vec<&PaneInfo> = snap
                .panes
                .iter()
                .filter(|p| p.tab_id == stray.tab_id && owned.contains(&p.pane_id))
                .collect();
            let Some((src, dir)) = pick_source(&snap, stray, &sibs) else {
                println!("adopt: stray {} has no mirror sibling — leaving it local", stray.pane_id);
                continue;
            };
            let Some(rid) = pane_rid.get(&src.pane_id) else { continue };
            let cwd = rsnap
                .panes
                .iter()
                .find(|p| &&p.pane_id == rid)
                .and_then(|p| p.foreground_cwd.clone().or_else(|| p.cwd.clone()));
            api.request(
                "pane.split",
                json!({ "target_pane_id": rid, "direction": dir, "cwd": cwd, "focus": false }),
            )
            .await?;
            // only after the remote split exists does closing the stray lose nothing
            let _ = local.request("pane.close", json!({ "pane_id": stray.pane_id })).await;
            println!(
                "adopt: replaced local pane {} with a remote {dir} split of {rid} on {}",
                stray.pane_id, host.name
            );
        }
    }
    Ok(())
}
