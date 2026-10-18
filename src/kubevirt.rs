//! The parts of KubeVirt's API this needs: which VMs run on a node, and how
//! their migrations are getting on. Everything else in the objects is ignored.

use kube::CustomResource;
use serde::{Deserialize, Serialize};

/// Label KubeVirt keeps on every VMI with the node it runs on.
pub const NODE_NAME_LABEL: &str = "kubevirt.io/nodeName";

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, Default)]
#[kube(
    group = "kubevirt.io",
    version = "v1",
    kind = "VirtualMachineInstance",
    namespaced,
    schema = "disabled"
)]
pub struct VirtualMachineInstanceSpec {}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, Default)]
#[kube(
    group = "kubevirt.io",
    version = "v1",
    kind = "VirtualMachineInstanceMigration",
    namespaced,
    status = "MigrationStatus",
    schema = "disabled"
)]
#[serde(rename_all = "camelCase")]
pub struct VirtualMachineInstanceMigrationSpec {
    pub vmi_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MigrationStatus {
    #[serde(default)]
    pub phase: MigrationPhase,
}

/// `Scheduling` is the one that matters here: the target pod exists but is not
/// running, usually because no node can take it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MigrationPhase {
    #[default]
    Pending,
    Scheduling,
    Scheduled,
    PreparingTarget,
    TargetReady,
    Running,
    Succeeded,
    Failed,
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_migration() {
        let m: VirtualMachineInstanceMigration = serde_json::from_value(serde_json::json!({
            "apiVersion": "kubevirt.io/v1",
            "kind": "VirtualMachineInstanceMigration",
            "metadata": {"name": "kubevirt-evacuation-abcde", "namespace": "vms"},
            "spec": {"vmiName": "db", "addedNodeSelector": {"a": "b"}},
            "status": {"phase": "Scheduling", "phaseTransitionTimestamps": []}
        }))
        .unwrap();
        assert_eq!(m.spec.vmi_name, "db");
        assert_eq!(m.status.unwrap().phase, MigrationPhase::Scheduling);
    }

    #[test]
    fn phases_from_newer_kubevirt_are_unknown() {
        let s: MigrationStatus = serde_json::from_value(serde_json::json!({"phase": "WaitingForSync"})).unwrap();
        assert_eq!(s.phase, MigrationPhase::Unknown);
        let s: MigrationStatus = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(s.phase, MigrationPhase::Pending);
    }
}
