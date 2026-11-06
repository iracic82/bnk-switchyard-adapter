#!/usr/bin/env bash
# The lab's conveniences, safe to run any number of times: the logins as Instruqt runtime variables (the
# assignments show them under "Logins for this lab"), and the Topology tab's service on :8092.
# Runs at the end of setup-lab and at the start of every challenge, so a lab heals itself.
set -uo pipefail
export PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/snap/bin:/root/.local/bin"
LAB_ROOT=${LAB_ROOT:-/root/lab}; SY_ROOT=${SY_ROOT:-/root/switchyard}; SECRETS_DIR=${SECRETS_DIR:-/root/.bnk}
export KUBECONFIG=${KUBECONFIG:-$LAB_ROOT/clab/kubeconfig}
HERE="$SY_ROOT/instruqt/bnk-switchyard-routing/scripts"

# Runtime variables can be set only from lifecycle scripts (setup-lab, a challenge's setup-lab).
AGENT=$(command -v agent || true)
[ -n "$AGENT" ] || { [ -x /opt/instruqt/bin/agent ] && AGENT=/opt/instruqt/bin/agent; }
# outside a lifecycle script (a learner's terminal) the agent has no token and cannot set variables
[ -n "${INSTRUQT_AUTH_TOKEN:-}" ] || AGENT=""
if [ -n "$AGENT" ]; then
  u=$(kubectl -n f5-bnk get secret f5-aigateway-admin -o jsonpath='{.data.username}' 2>/dev/null | base64 -d)
  p=$(kubectl -n f5-bnk get secret f5-aigateway-admin -o jsonpath='{.data.password}' 2>/dev/null | base64 -d)
  [ -n "$u" ] && "$AGENT" variable set AIGW_USER "$u"
  [ -n "$p" ] && "$AGENT" variable set AIGW_PASSWORD "$p"
  for k in eng pilot dynamo; do
    f="$SECRETS_DIR/aigw/switchyard-$k-key.key"
    [ -s "$f" ] && "$AGENT" variable set "SY_$(echo "$k" | tr a-z A-Z)_KEY" "$(cat "$f")"
  done
else
  echo "   logins not stored for the assignments from here (only setup scripts can); sy logins prints them"
fi

# the sy helper (sy logins, auto-dynamo) at the version of this checkout
install -m 0755 "$HERE/sy" /usr/local/bin/sy

# the Topology tab
if [ ! -f /etc/systemd/system/sy-topology.service ] || ! systemctl is-active --quiet sy-topology; then
  cat > /etc/systemd/system/sy-topology.service <<UNIT
[Unit]
Description=Switchyard lab topology page on :8092
After=network-online.target docker.service
[Service]
Environment=KUBECONFIG=$KUBECONFIG SECRETS_DIR=$SECRETS_DIR
ExecStart=/usr/bin/python3 $HERE/topology.py --bind 0.0.0.0 --port 8092
Restart=always
RestartSec=2
[Install]
WantedBy=multi-user.target
UNIT
  systemctl daemon-reload
  systemctl enable --now sy-topology >/dev/null 2>&1
  systemctl restart sy-topology
fi
exit 0
