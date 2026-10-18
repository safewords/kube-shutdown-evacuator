use std::io::IsTerminal;

use clap::{Parser, Subcommand};
use futures::StreamExt;
use k8s_openapi::api::core::v1::Node;
use kube::Api;
use tracing::{error, info, warn};

use kube_shutdown_evacuator::config::Settings;
use kube_shutdown_evacuator::evacuate::Evacuation;
use kube_shutdown_evacuator::logind::ManagerProxy;
use kube_shutdown_evacuator::node;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(flatten)]
    settings: Settings,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Hold a shutdown-delay lock and evacuate the node when a shutdown starts.
    Watch,
    /// Cordon and drain the node now.
    Evacuate {
        /// Only drain pods matching this label selector, to try it on one workload.
        #[arg(long)]
        pod_selector: Option<String>,
    },
    /// Uncordon the node, if an evacuation cordoned it.
    Restore,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,kube_client=warn,kube_runtime=warn".into()),
        )
        // The journal keeps its own timestamps, and has no use for colours.
        .with_ansi(std::io::stdout().is_terminal())
        .without_time()
        .init();
    let cli = Cli::parse();
    let client = cli.settings.client().await?;
    let nodes: Api<Node> = Api::all(client.clone());
    let settings = cli.settings;

    match cli.command {
        Command::Watch => watch(client, settings).await,
        Command::Evacuate { pod_selector } => Evacuation::new(client, settings, pod_selector).run().await,
        Command::Restore => node::restore(&nodes, &settings.node, &settings.annotation).await,
    }
}

async fn watch(client: kube::Client, settings: Settings) -> anyhow::Result<()> {
    let bus = zbus::Connection::system().await?;
    let logind = ManagerProxy::new(&bus).await?;
    // Subscribed before the lock is taken, so no shutdown can slip between.
    let mut signals = logind.receive_prepare_for_shutdown().await?;
    let nodes: Api<Node> = Api::all(client.clone());

    let (n, s) = (nodes.clone(), settings.clone());
    tokio::spawn(async move {
        if let Err(e) = node::restore(&n, &s.node, &s.annotation).await {
            error!(error = %e, "uncordoning failed");
        }
    });

    let mut warned = false;
    loop {
        let lock = logind.delay_shutdown().await?;
        info!(node = %settings.node, "holding a shutdown-delay lock");
        if !warned {
            warned = true;
            if logind
                .list_inhibitors()
                .await?
                .iter()
                .any(|i| i.is_kubelet_shutdown_lock())
            {
                warn!(
                    "kubelet graceful node shutdown is on: it kills pods, VMs included, the moment a shutdown \
                     starts. Set shutdownGracePeriod and shutdownGracePeriodCriticalPods to 0s."
                );
            }
        }

        // Wait for a shutdown to start.
        loop {
            let signal = signals
                .next()
                .await
                .ok_or_else(|| anyhow::anyhow!("lost logind's signals"))?;
            if signal.args()?.start {
                break;
            }
        }
        info!("shutdown requested");
        if let Err(e) = Evacuation::new(client.clone(), settings.clone(), None).run().await {
            error!(error = %e, "evacuation incomplete");
        }
        drop(lock);
        info!("released the shutdown-delay lock");

        // Normally the machine now goes down. A cancelled shutdown brings the
        // node back into service and starts over.
        loop {
            let signal = signals
                .next()
                .await
                .ok_or_else(|| anyhow::anyhow!("lost logind's signals"))?;
            if !signal.args()?.start {
                break;
            }
        }
        info!("shutdown cancelled");
        node::restore(&nodes, &settings.node, &settings.annotation).await?;
    }
}
