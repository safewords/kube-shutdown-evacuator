//! Cordoning the node on the way down, and uncordoning it on the way back up,
//! but only when the cordon was ours.

use std::time::Duration;

use anyhow::Context;
use k8s_openapi::api::core::v1::Node;
use kube::Api;
use kube::api::{Patch, PatchParams};
use serde_json::json;
use tracing::info;

const POLL: Duration = Duration::from_secs(5);

pub fn is_cordoned(node: &Node) -> bool {
    node.spec.as_ref().and_then(|s| s.unschedulable).unwrap_or(false)
}

pub fn is_ready(node: &Node) -> bool {
    node.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|c| c.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
}

pub fn cordoned_by_us(node: &Node, annotation: &str) -> bool {
    node.metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(annotation))
}

/// Cordons the node. A node that was schedulable is also annotated, so that
/// [`restore`] knows the cordon is ours to undo; one that a person or another
/// controller had cordoned is left for them.
pub async fn cordon(nodes: &Api<Node>, name: &str, annotation: &str) -> anyhow::Result<()> {
    loop {
        let node = nodes.get(name).await.context("reading the node")?;
        if is_cordoned(&node) {
            return Ok(());
        }
        // The resourceVersion makes this fail rather than claim a cordon
        // someone else made in the meantime.
        let patch = json!({
            "metadata": {
                "resourceVersion": node.metadata.resource_version,
                "annotations": { annotation: jiff::Timestamp::now().to_string() },
            },
            "spec": { "unschedulable": true },
        });
        match nodes.patch(name, &PatchParams::default(), &Patch::Merge(&patch)).await {
            Ok(_) => {
                info!(node = name, "cordoned");
                return Ok(());
            }
            Err(kube::Error::Api(e)) if e.code == 409 => continue,
            Err(e) => return Err(e).context("cordoning the node"),
        }
    }
}

/// Waits for the node to be Ready, then uncordons it if [`cordon`] did the
/// cordoning.
pub async fn restore(nodes: &Api<Node>, name: &str, annotation: &str) -> anyhow::Result<()> {
    let node = loop {
        match nodes.get_opt(name).await {
            Ok(Some(node)) if !cordoned_by_us(&node, annotation) => return Ok(()),
            Ok(Some(node)) if is_ready(&node) => break node,
            _ => tokio::time::sleep(POLL).await,
        }
    };
    let patch = json!({
        "metadata": {
            "resourceVersion": node.metadata.resource_version,
            "annotations": { annotation: null },
        },
        "spec": { "unschedulable": false },
    });
    nodes
        .patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
        .context("uncordoning the node")?;
    info!(node = name, "uncordoned");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(v: serde_json::Value) -> Node {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn reads_node_state() {
        let n = node(json!({
            "metadata": {"name": "n", "annotations": {"kube-shutdown-evacuator/cordoned-at": "x"}},
            "spec": {"unschedulable": true},
            "status": {"conditions": [{"type": "Ready", "status": "True"}]},
        }));
        assert!(is_cordoned(&n));
        assert!(is_ready(&n));
        assert!(cordoned_by_us(&n, "kube-shutdown-evacuator/cordoned-at"));

        let n = node(json!({
            "metadata": {"name": "n"},
            "status": {"conditions": [{"type": "Ready", "status": "Unknown"}]},
        }));
        assert!(!is_cordoned(&n));
        assert!(!is_ready(&n));
        assert!(!cordoned_by_us(&n, "kube-shutdown-evacuator/cordoned-at"));
    }
}
