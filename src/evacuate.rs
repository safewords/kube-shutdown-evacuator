//! Emptying the node: evict what can move, give up on VMs that have nowhere
//! to go, and delete whatever is left when time runs out.
//!
//! Evictions go through the Eviction API, so PodDisruptionBudgets hold and
//! KubeVirt's eviction webhook turns the eviction of a VM's launcher pod into
//! whatever the VM's evictionStrategy says: a live migration, or a shutdown
//! after which its VirtualMachine starts it elsewhere.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{DeleteParams, EvictParams, ListParams};
use kube::{Api, Client, ResourceExt};
use tracing::{info, warn};

use crate::config::Settings;
use crate::kubevirt::{MigrationPhase, NODE_NAME_LABEL, VirtualMachineInstance, VirtualMachineInstanceMigration};
use crate::node;

const ROUND: Duration = Duration::from_secs(5);
const MIRROR_POD_ANNOTATION: &str = "kubernetes.io/config.mirror";

/// What draining does with a pod on the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PodAction {
    /// DaemonSet and static pods stay until the machine stops. The CSI node
    /// plugins and the CNI are among them, and migrations need both.
    Leave,
    /// Finished; nothing to protect, so it is simply deleted.
    Delete,
    Evict,
}

pub fn pod_action(pod: &Pod) -> PodAction {
    let daemonset = pod
        .owner_references()
        .iter()
        .any(|o| o.kind == "DaemonSet" && o.controller == Some(true));
    if daemonset || pod.annotations().contains_key(MIRROR_POD_ANNOTATION) {
        return PodAction::Leave;
    }
    match pod.status.as_ref().and_then(|s| s.phase.as_deref()) {
        Some("Succeeded" | "Failed") => PodAction::Delete,
        _ => PodAction::Evict,
    }
}

/// How long each migration has been waiting for a target pod.
#[derive(Debug, Default)]
pub struct StuckMigrations {
    scheduling_since: HashMap<(String, String), Instant>,
}

impl StuckMigrations {
    /// Records a migration's phase; true once it has been `Scheduling` for
    /// `after`.
    pub fn observe(
        &mut self,
        namespace: &str,
        name: &str,
        phase: MigrationPhase,
        now: Instant,
        after: Duration,
    ) -> bool {
        let key = (namespace.to_owned(), name.to_owned());
        if phase != MigrationPhase::Scheduling {
            self.scheduling_since.remove(&key);
            return false;
        }
        let since = *self.scheduling_since.entry(key).or_insert(now);
        now.duration_since(since) >= after
    }
}

pub struct Evacuation {
    client: Client,
    settings: Settings,
    /// Limits the drain to matching pods, for trying it out on one workload.
    pod_selector: Option<String>,
}

impl Evacuation {
    pub fn new(client: Client, settings: Settings, pod_selector: Option<String>) -> Self {
        Self {
            client,
            settings,
            pod_selector,
        }
    }

    pub async fn run(&self) -> anyhow::Result<()> {
        let name = &self.settings.node;
        info!(node = %name, "evacuating");
        let nodes: Api<Node> = Api::all(self.client.clone());
        node::cordon(&nodes, name, &self.settings.annotation).await?;

        let deadline = Instant::now() + self.settings.drain_timeout;
        let mut stuck = StuckMigrations::default();
        loop {
            match self.evict_round().await {
                Ok(0) => {
                    info!(node = %name, "drained");
                    return Ok(());
                }
                Ok(left) => info!(pods = left, "waiting for pods to leave"),
                Err(e) => warn!(error = %e, "eviction round failed"),
            }
            if let Err(e) = self.give_up_stuck_migrations(&mut stuck).await {
                warn!(error = %e, "checking migrations failed");
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(ROUND).await;
        }

        warn!(
            timeout = self.settings.drain_timeout.as_secs(),
            "drain did not finish; deleting what is left in place"
        );
        for (namespace, vmi) in self.vmis_here().await? {
            self.delete_vmi(&namespace, &vmi).await;
        }
        self.delete_remaining().await
    }

    async fn pods_here(&self) -> kube::Result<Vec<Pod>> {
        let mut params = ListParams::default().fields(&format!("spec.nodeName={}", self.settings.node));
        if let Some(selector) = &self.pod_selector {
            params = params.labels(selector);
        }
        Ok(Api::<Pod>::all(self.client.clone()).list(&params).await?.items)
    }

    /// Asks every pod still to go to leave; returns how many have not left yet.
    async fn evict_round(&self) -> kube::Result<usize> {
        let mut left = 0;
        for pod in self.pods_here().await? {
            let action = pod_action(&pod);
            if action == PodAction::Leave {
                continue;
            }
            left += 1;
            if pod.metadata.deletion_timestamp.is_some() {
                continue;
            }
            let namespace = pod.namespace().unwrap_or_default();
            let api: Api<Pod> = Api::namespaced(self.client.clone(), &namespace);
            let result = match action {
                PodAction::Evict => api.evict(&pod.name_any(), &EvictParams::default()).await.map(drop),
                _ => api.delete(&pod.name_any(), &DeleteParams::default()).await.map(drop),
            };
            match result {
                Ok(()) => info!(pod = %format!("{namespace}/{}", pod.name_any()), "evicted"),
                // 429: a disruption budget, or KubeVirt migrating the VM instead.
                Err(kube::Error::Api(e)) if e.code == 404 || e.code == 429 => {}
                Err(e) => warn!(pod = %format!("{namespace}/{}", pod.name_any()), error = %e, "eviction failed"),
            }
        }
        Ok(left)
    }

    /// `(namespace, name)` of each VMI on this node; none without KubeVirt.
    async fn vmis_here(&self) -> kube::Result<Vec<(String, String)>> {
        let params = ListParams::default().labels(&format!("{NODE_NAME_LABEL}={}", self.settings.node));
        match Api::<VirtualMachineInstance>::all(self.client.clone())
            .list(&params)
            .await
        {
            Ok(list) => Ok(list
                .items
                .iter()
                .map(|v| (v.namespace().unwrap_or_default(), v.name_any()))
                .collect()),
            Err(kube::Error::Api(e)) if e.code == 404 => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Deletes the VMI of each migration that has waited `stuck_after` for a
    /// target pod: the guest shuts down cleanly here, and its VirtualMachine
    /// starts it wherever it is still allowed to run.
    async fn give_up_stuck_migrations(&self, stuck: &mut StuckMigrations) -> kube::Result<()> {
        let here: HashSet<_> = self.vmis_here().await?.into_iter().collect();
        if here.is_empty() {
            return Ok(());
        }
        let migrations = Api::<VirtualMachineInstanceMigration>::all(self.client.clone())
            .list(&ListParams::default())
            .await?;
        let now = Instant::now();
        for m in migrations.items {
            let namespace = m.namespace().unwrap_or_default();
            if !here.contains(&(namespace.clone(), m.spec.vmi_name.clone())) {
                continue;
            }
            let phase = m.status.as_ref().map(|s| s.phase).unwrap_or_default();
            if stuck.observe(&namespace, &m.name_any(), phase, now, self.settings.stuck_after) {
                warn!(
                    vmi = %format!("{namespace}/{}", m.spec.vmi_name),
                    after = self.settings.stuck_after.as_secs(),
                    "migration has no target; shutting the VM down so it restarts elsewhere"
                );
                self.delete_vmi(&namespace, &m.spec.vmi_name).await;
                let api: Api<VirtualMachineInstanceMigration> = Api::namespaced(self.client.clone(), &namespace);
                let _ = api.delete(&m.name_any(), &DeleteParams::default()).await;
            }
        }
        Ok(())
    }

    async fn delete_vmi(&self, namespace: &str, name: &str) {
        let api: Api<VirtualMachineInstance> = Api::namespaced(self.client.clone(), namespace);
        match api.delete(name, &DeleteParams::default()).await {
            Ok(_) => info!(vmi = %format!("{namespace}/{name}"), "shutting down"),
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => warn!(vmi = %format!("{namespace}/{name}"), error = %e, "deleting VMI failed"),
        }
    }

    /// Deletes every remaining pod, bypassing eviction, and waits up to
    /// `final_timeout` for them to finish terminating.
    async fn delete_remaining(&self) -> anyhow::Result<()> {
        let deadline = Instant::now() + self.settings.final_timeout;
        loop {
            let pods: Vec<Pod> = self
                .pods_here()
                .await?
                .into_iter()
                .filter(|p| pod_action(p) != PodAction::Leave)
                .collect();
            if pods.is_empty() {
                info!(node = %self.settings.node, "drained");
                return Ok(());
            }
            if Instant::now() >= deadline {
                anyhow::bail!("{} pod(s) still terminating; letting the shutdown go on", pods.len());
            }
            for pod in pods.iter().filter(|p| p.metadata.deletion_timestamp.is_none()) {
                let api: Api<Pod> = Api::namespaced(self.client.clone(), &pod.namespace().unwrap_or_default());
                let _ = api.delete(&pod.name_any(), &DeleteParams::default()).await;
            }
            tokio::time::sleep(ROUND).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pod(v: serde_json::Value) -> Pod {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn leaves_daemonsets_and_static_pods() {
        let ds = pod(json!({"metadata": {"name": "cilium-x", "ownerReferences": [
            {"apiVersion": "apps/v1", "kind": "DaemonSet", "name": "cilium", "uid": "u", "controller": true}
        ]}}));
        assert_eq!(pod_action(&ds), PodAction::Leave);
        let mirror = pod(json!({"metadata": {"name": "etcd", "annotations": {"kubernetes.io/config.mirror": "h"}}}));
        assert_eq!(pod_action(&mirror), PodAction::Leave);
    }

    #[test]
    fn evicts_everything_else() {
        let launcher = pod(json!({
            "metadata": {"name": "virt-launcher-db-abcde", "ownerReferences": [
                {"apiVersion": "kubevirt.io/v1", "kind": "VirtualMachineInstance", "name": "db", "uid": "u", "controller": true}
            ]},
            "status": {"phase": "Running"},
        }));
        assert_eq!(pod_action(&launcher), PodAction::Evict);
        // Unmanaged pods too: the machine is going down regardless.
        assert_eq!(
            pod_action(&pod(json!({"metadata": {"name": "bare"}}))),
            PodAction::Evict
        );
        let done = pod(json!({"metadata": {"name": "job-x"}, "status": {"phase": "Succeeded"}}));
        assert_eq!(pod_action(&done), PodAction::Delete);
    }

    #[test]
    fn a_migration_is_stuck_after_scheduling_for_long_enough() {
        let mut stuck = StuckMigrations::default();
        let t0 = Instant::now();
        let after = Duration::from_secs(60);
        assert!(!stuck.observe("ns", "m", MigrationPhase::Pending, t0, after));
        assert!(!stuck.observe(
            "ns",
            "m",
            MigrationPhase::Scheduling,
            t0 + Duration::from_secs(10),
            after
        ));
        assert!(!stuck.observe(
            "ns",
            "m",
            MigrationPhase::Scheduling,
            t0 + Duration::from_secs(69),
            after
        ));
        assert!(stuck.observe(
            "ns",
            "m",
            MigrationPhase::Scheduling,
            t0 + Duration::from_secs(70),
            after
        ));
    }

    #[test]
    fn leaving_scheduling_resets_the_clock() {
        let mut stuck = StuckMigrations::default();
        let t0 = Instant::now();
        let after = Duration::from_secs(60);
        stuck.observe("ns", "m", MigrationPhase::Scheduling, t0, after);
        assert!(!stuck.observe("ns", "m", MigrationPhase::Running, t0 + Duration::from_secs(30), after));
        assert!(!stuck.observe(
            "ns",
            "m",
            MigrationPhase::Scheduling,
            t0 + Duration::from_secs(80),
            after
        ));
        // Per migration, not per VM.
        assert!(!stuck.observe(
            "ns",
            "other",
            MigrationPhase::Scheduling,
            t0 + Duration::from_secs(80),
            after
        ));
    }
}
