use std::num::ParseIntError;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use kube::config::{KubeConfigOptions, Kubeconfig};

/// Where distributions keep a kubeconfig that can drain the node, in the order
/// they are tried when none is given.
const KUBECONFIG_CANDIDATES: &[&str] = &[
    "/etc/kube-shutdown-evacuator/kubeconfig",
    "/etc/rancher/k3s/k3s.yaml",
    "/etc/rancher/rke2/rke2.yaml",
    "/etc/kubernetes/admin.conf",
];

#[derive(Debug, Clone, clap::Args)]
pub struct Settings {
    /// This machine's Node name.
    #[arg(long, env = "NODE_NAME", default_value_t = hostname())]
    pub node: String,

    /// Kubeconfig to use; without it, the first of the usual k3s, RKE2 and
    /// kubeadm locations that exists.
    #[arg(long, env = "KUBECONFIG")]
    pub kubeconfig: Option<PathBuf>,

    /// Seconds for evictions and migrations, before what's left is deleted in place.
    #[arg(long, env = "DRAIN_TIMEOUT", default_value = "420", value_parser = seconds)]
    pub drain_timeout: Duration,

    /// Seconds for that deletion: the longest terminationGracePeriodSeconds, plus a margin.
    #[arg(long, env = "FINAL_TIMEOUT", default_value = "360", value_parser = seconds)]
    pub final_timeout: Duration,

    /// Seconds a migration may wait for a target pod before its VM is shut down instead.
    #[arg(long, env = "STUCK_AFTER", default_value = "60", value_parser = seconds)]
    pub stuck_after: Duration,

    /// Node annotation marking a cordon this tool made, and may undo.
    #[arg(long, env = "ANNOTATION", default_value = "kube-shutdown-evacuator/cordoned-at")]
    pub annotation: String,
}

fn seconds(s: &str) -> Result<Duration, ParseIntError> {
    s.parse().map(Duration::from_secs)
}

fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

impl Settings {
    pub async fn client(&self) -> anyhow::Result<kube::Client> {
        let path = match &self.kubeconfig {
            Some(path) => path.clone(),
            None => KUBECONFIG_CANDIDATES
                .iter()
                .map(Path::new)
                .find(|p| p.exists())
                .map(Path::to_path_buf)
                .context("no kubeconfig: pass --kubeconfig, or set KUBECONFIG")?,
        };
        let kubeconfig = Kubeconfig::read_from(&path).with_context(|| format!("reading {}", path.display()))?;
        let config = kube::Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default()).await?;
        Ok(kube::Client::try_from(config)?)
    }
}
