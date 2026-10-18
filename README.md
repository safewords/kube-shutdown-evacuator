# kube-shutdown-evacuator

[![ci](https://github.com/safewords/kube-shutdown-evacuator/actions/workflows/ci.yml/badge.svg)](https://github.com/safewords/kube-shutdown-evacuator/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Live-migrates a node's KubeVirt VMs, and drains the rest, before the machine powers off.**

Tell a Kubernetes node to shut down or reboot (`systemctl poweroff`, `reboot`, the
power button, a UPS daemon) and kube-shutdown-evacuator holds the shutdown while it
cordons and drains the node. VMs that can live-migrate do; VMs that can't are shut
down cleanly and started again on another node. Once the node is empty, the shutdown
continues. When the machine comes back, the node is uncordoned.

It is a bash script and a systemd unit, run on each node. There is nothing to deploy
in the cluster.

## How it works

The service takes a logind *delay* inhibitor lock at boot. When a shutdown starts,
logind sends `PrepareForShutdown` and then waits, up to `InhibitDelayMaxSec`, for
delay locks to be released. In that window, nothing has been stopped yet: the
kubelet, the container runtime, the CSI node plugins and the network are all up.
kube-shutdown-evacuator then:

1. Cordons the node, and notes in an annotation that it did.
2. Runs `kubectl drain`. Drain uses the Eviction API, so PodDisruptionBudgets are
   respected and KubeVirt's eviction webhook decides what happens to each VM (see
   [KubeVirt](#kubevirt)).
3. Watches the node's VM migrations. One whose target pod has not been scheduled
   after `STUCK_AFTER` seconds (the VM is pinned to this node, or no other node has
   room or a compatible CPU) is cancelled and the VMI is deleted. The guest gets a
   clean ACPI shutdown, and its VirtualMachine controller starts it wherever it is
   still allowed to run.
4. If the drain has not finished after `DRAIN_TIMEOUT`, deletes whatever is left,
   without eviction, giving each pod its termination grace period.
5. Releases the lock, and the shutdown continues.

On the next boot the service uncordons the node once it is Ready, but only if the
annotation says it cordoned the node itself. A node cordoned by a person or by
another controller stays cordoned.

## Requirements

- systemd 240 or newer (`busctl monitor --json`), with logind.
- `kubectl` on the node, and a kubeconfig that can drain it. The script looks for
  one in this order: `$KUBECONFIG`, `/etc/kube-shutdown-evacuator/kubeconfig`,
  `/etc/rancher/k3s/k3s.yaml`, `/etc/rancher/rke2/rke2.yaml`,
  `/etc/kubernetes/admin.conf`.
- **Kubelet graceful node shutdown turned off.** See [below](#kubelet-graceful-node-shutdown).
- KubeVirt is optional. Without it, this is a plain drain on shutdown.

## Install

On each node, as root:

```sh
git clone https://github.com/safewords/kube-shutdown-evacuator
cd kube-shutdown-evacuator
./install.sh
```

This installs `/usr/local/bin/kube-shutdown-evacuator`, the
`kube-shutdown-evacuator.service` unit, and a logind drop-in that raises
`InhibitDelayMaxSec` to 900 seconds. Then it starts the service and checks that the
lock is held. `./install.sh uninstall` removes all of it.

Check it with:

```sh
systemd-inhibit --list          # kube-shutdown-evacuator ... shutdown ... delay
journalctl -u kube-shutdown-evacuator
```

### Nodes without an admin kubeconfig

Control-plane nodes usually have one; workers usually don't. `deploy/rbac.yaml`
creates a ServiceAccount that can drain nodes. Apply it once, then write a
kubeconfig for it on each worker:

```sh
kubectl apply -f deploy/rbac.yaml
server=$(kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}')
token=$(kubectl -n kube-system get secret kube-shutdown-evacuator-token -o jsonpath='{.data.token}' | base64 -d)
kubectl -n kube-system get secret kube-shutdown-evacuator-token -o jsonpath='{.data.ca\.crt}' | base64 -d > ca.crt
KUBECONFIG=kubeconfig kubectl config set-cluster cluster --server="$server" --certificate-authority=ca.crt --embed-certs
KUBECONFIG=kubeconfig kubectl config set-credentials evacuator --token="$token"
KUBECONFIG=kubeconfig kubectl config set-context default --cluster=cluster --user=evacuator
KUBECONFIG=kubeconfig kubectl config use-context default
# then, on the node: install -Dm600 kubeconfig /etc/kube-shutdown-evacuator/kubeconfig
```

The token can evict any pod on any node. Keep the file readable by root only.

## Kubelet graceful node shutdown

Kubelet's [graceful node shutdown](https://kubernetes.io/docs/concepts/cluster-administration/node-shutdown/)
uses the same logind lock, and when the shutdown starts it terminates every pod on
the node. It runs at the same moment as kube-shutdown-evacuator, so it stops VMs
that were about to migrate. Turn it off by setting both of these to `0s` (the
upstream default) and restarting the kubelet:

```yaml
shutdownGracePeriod: 0s
shutdownGracePeriodCriticalPods: 0s
```

- **kubeadm:** in `/var/lib/kubelet/config.yaml`, then `systemctl restart kubelet`.
- **k3s / RKE2:** in a drop-in under `/var/lib/rancher/<k3s|rke2>/agent/etc/kubelet.conf.d/`
  (or remove the drop-in that set them), then restart `k3s`/`k3s-agent` or
  `rke2-server`/`rke2-agent`.

Nothing is lost: a drain terminates every pod gracefully too, and it lets each
controller recreate its pods elsewhere first.

`install.sh` warns when the kubelet still holds a shutdown lock.

## KubeVirt

What draining does to a VM depends on its eviction strategy, set cluster-wide in the
KubeVirt CR or per VM in `spec.template.spec.evictionStrategy`. For "live-migrate
if possible, otherwise restart elsewhere", use `LiveMigrateIfPossible`:

```sh
kubectl patch kubevirt -n kubevirt kubevirt --type=merge -p \
  '{"spec":{"configuration":{"evictionStrategy":"LiveMigrateIfPossible","migrations":{"allowAutoConverge":true}}}}'
```

| Strategy | On drain |
| --- | --- |
| `LiveMigrateIfPossible` | Migrates if the VMI is live-migratable; otherwise the VM is shut down and restarted elsewhere. |
| `LiveMigrate` | Migrates. A non-migratable VMI blocks the drain until `DRAIN_TIMEOUT`, then is shut down. |
| `None` | Shut down and restarted elsewhere. |
| `External` | Left to an external controller; kube-shutdown-evacuator waits until `DRAIN_TIMEOUT`. |

A VM only restarts elsewhere if its `runStrategy` brings it back (`Always`,
`RerunOnFailure`) and its scheduling constraints allow another node. If they don't,
it shuts down cleanly and starts again when this node returns.

For live migration to succeed, the VM's disks must be on `ReadWriteMany` storage
(or use a migratable volume type), and another node must be able to run its CPU
model: `host-passthrough` VMs can only move to an identical CPU.

## Configuration

Set these in `/etc/default/kube-shutdown-evacuator`:

| Variable | Default | |
| --- | --- | --- |
| `NODE` | `$(hostname)` | The Node name, if it differs from the hostname. |
| `KUBECONFIG` | first one found, see [Requirements](#requirements) | |
| `DRAIN_TIMEOUT` | `420` | Seconds for evictions and migrations before what's left is deleted in place. |
| `FINAL_TIMEOUT` | `360` | Seconds for that deletion: the longest `terminationGracePeriodSeconds` you run, plus a margin. |
| `STUCK_AFTER` | `60` | Seconds a migration may wait for a target pod before the VM is shut down instead. |
| `ANNOTATION` | `kube-shutdown-evacuator/cordoned-at` | Marks a node this tool cordoned. |

Keep `InhibitDelayMaxSec` in `/etc/systemd/logind.conf.d/90-kube-shutdown-evacuator.conf`
above `DRAIN_TIMEOUT + FINAL_TIMEOUT`. When it runs out, logind shuts down
regardless.

## Trying it out

`evacuate` and `restore` run the same steps by hand, without shutting anything down.
Extra arguments go to `kubectl drain`, so you can limit a test to one VM:

```sh
kube-shutdown-evacuator evacuate --pod-selector=vm.kubevirt.io/name=my-test-vm
kube-shutdown-evacuator restore
```

## Limitations

- Only shutdowns that go through logind are held: `systemctl poweroff`/`reboot`,
  `shutdown`, the power button, and anything that calls them. `systemctl poweroff --force`,
  `reboot -f`, SysRq, a kernel panic and losing power are not.
- A shutdown cancelled after the evacuation has started leaves the node cordoned,
  and the service stopped. `systemctl start kube-shutdown-evacuator` uncordons it.
- Each node evacuates on its own. Shutting down several at once can leave the VMs
  nowhere to go.

## License

[Apache-2.0](LICENSE)
