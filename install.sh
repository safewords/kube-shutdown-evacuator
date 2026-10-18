#!/bin/bash
# Installs kube-shutdown-evacuator on this machine. Run as root on each node:
#   ./install.sh             install, or update, and start it
#   ./install.sh uninstall   stop and remove it
set -eu
cd "$(dirname "$0")"

if [ "${1:-}" = uninstall ]; then
    systemctl disable --now kube-shutdown-evacuator.service 2>/dev/null || true
    rm -f /usr/local/bin/kube-shutdown-evacuator \
        /etc/systemd/system/kube-shutdown-evacuator.service \
        /etc/systemd/logind.conf.d/90-kube-shutdown-evacuator.conf
    systemctl daemon-reload
    systemctl kill -s HUP systemd-logind
    exit 0
fi

install -Dm755 bin/kube-shutdown-evacuator /usr/local/bin/kube-shutdown-evacuator
install -Dm644 systemd/kube-shutdown-evacuator.service /etc/systemd/system/kube-shutdown-evacuator.service
install -Dm644 systemd/90-kube-shutdown-evacuator.conf /etc/systemd/logind.conf.d/90-kube-shutdown-evacuator.conf
# logind rereads its configuration on SIGHUP.
systemctl kill -s HUP systemd-logind
systemctl daemon-reload
systemctl enable kube-shutdown-evacuator.service
systemctl restart kube-shutdown-evacuator.service

sleep 2
systemd-inhibit --list --no-pager | grep -q kube-shutdown-evacuator ||
    echo "warning: kube-shutdown-evacuator holds no shutdown lock; see journalctl -u kube-shutdown-evacuator" >&2
if systemd-inhibit --list --no-pager | grep -qw kubelet; then
    cat >&2 <<'WARN'
warning: kubelet holds a shutdown lock too, so graceful node shutdown is on. It
kills pods (VMs included) at the same moment this starts migrating them. Set
shutdownGracePeriod and shutdownGracePeriodCriticalPods to 0s in the kubelet
configuration and restart kubelet; the README says where for each distribution.
WARN
fi
