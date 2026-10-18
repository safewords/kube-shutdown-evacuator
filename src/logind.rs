//! systemd-logind: the shutdown-delay lock, and the signal that a shutdown has
//! started.
//!
//! logind sends `PrepareForShutdown(true)` when a shutdown or reboot begins and
//! then waits, up to `InhibitDelayMaxSec`, until every delay lock is released.
//! Nothing has been stopped yet in that window, which is when the node is
//! evacuated.

use zbus::proxy;
use zbus::zvariant::{OwnedFd, Type};

#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub trait Manager {
    /// Takes an inhibitor lock, held until the returned descriptor is closed.
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;

    fn list_inhibitors(&self) -> zbus::Result<Vec<Inhibitor>>;

    /// `start` is true when a shutdown begins, false when one is cancelled.
    #[zbus(signal)]
    fn prepare_for_shutdown(&self, start: bool) -> zbus::Result<()>;
}

#[derive(serde::Deserialize, Type, Debug, Clone, PartialEq, Eq)]
pub struct Inhibitor {
    pub what: String,
    pub who: String,
    pub why: String,
    pub mode: String,
    pub uid: u32,
    pub pid: u32,
}

impl Inhibitor {
    /// Kubelet's graceful node shutdown: it terminates every pod as soon as the
    /// shutdown starts, VMs included, racing the evacuation.
    pub fn is_kubelet_shutdown_lock(&self) -> bool {
        self.who == "kubelet" && self.mode == "delay" && self.what.split(':').any(|w| w == "shutdown")
    }
}

/// A held shutdown-delay lock. Dropping it lets the shutdown continue.
pub struct ShutdownLock(#[allow(dead_code)] OwnedFd);

impl ManagerProxy<'_> {
    pub async fn delay_shutdown(&self) -> zbus::Result<ShutdownLock> {
        self.inhibit(
            "shutdown",
            "kube-shutdown-evacuator",
            "Evacuating this Kubernetes node",
            "delay",
        )
        .await
        .map(ShutdownLock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock(who: &str, what: &str, mode: &str) -> Inhibitor {
        Inhibitor {
            what: what.into(),
            who: who.into(),
            why: String::new(),
            mode: mode.into(),
            uid: 0,
            pid: 1,
        }
    }

    #[test]
    fn spots_kubelet_graceful_shutdown() {
        assert!(lock("kubelet", "shutdown", "delay").is_kubelet_shutdown_lock());
        assert!(lock("kubelet", "sleep:shutdown", "delay").is_kubelet_shutdown_lock());
        assert!(!lock("kubelet", "sleep", "delay").is_kubelet_shutdown_lock());
        assert!(!lock("kubelet", "shutdown", "block").is_kubelet_shutdown_lock());
        assert!(!lock("kube-shutdown-evacuator", "shutdown", "delay").is_kubelet_shutdown_lock());
    }
}
