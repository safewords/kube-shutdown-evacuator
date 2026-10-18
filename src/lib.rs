//! Holds a Kubernetes node's shutdown until it has been evacuated: KubeVirt VMs
//! live-migrated where they can be, everything else drained.

pub mod config;
pub mod evacuate;
pub mod kubevirt;
pub mod logind;
pub mod node;
