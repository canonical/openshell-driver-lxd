# Demo: openshell-driver-lxd end to end

Manual walkthrough for exercising the full stack: driver → gateway → LXD
sandbox → supervisor → agent.

**Prerequisites built locally (no steps needed):**
- `openshell-sandbox` LXD image alias (`make sandbox-image` if missing)
- `openshell-driver-lxd` driver binary (`cargo build -p openshell-driver-lxd`)
- `openshell-gateway` binary at
  `/home/kadinsayani/git/lxd-dev/git/OpenShell/target/release/openshell-gateway`
  (build: `cd /home/kadinsayani/git/lxd-dev/git/OpenShell && cargo build --release -p openshell-server`)
- `openshell` CLI at
  `/home/kadinsayani/git/lxd-dev/git/OpenShell/target/release/openshell`
  (build: `cd /home/kadinsayani/git/lxd-dev/git/OpenShell && cargo build --release -p openshell-cli --no-default-features`)
- `grpcurl` installed

---

## 1. Build the sandbox image (if not already imported)

```bash
cd /home/kadinsayani/git/lxd-dev/git/openshell-driver-lxd
make sandbox-image
lxc image list openshell-sandbox   # should show one entry
```

Skip if `lxc image list openshell-sandbox` already shows an entry.

---

## 2. Start the driver

```bash
cd /home/kadinsayani/git/lxd-dev/git/openshell-driver-lxd
cargo build -p openshell-driver-lxd

rm -f /tmp/openshell-driver.sock
./target/debug/openshell-driver-lxd --socket /tmp/openshell-driver.sock &
```

Expected output: `INFO openshell_driver_lxd: Starting OpenShell LXD compute driver socket=/tmp/openshell-driver.sock`

---

## 3. Start the gateway

```bash
REPO=/home/kadinsayani/git/lxd-dev/git/openshell-driver-lxd
GW=/home/kadinsayani/git/lxd-dev/git/OpenShell/target/release/openshell-gateway

$GW \
  --bind-address 0.0.0.0 \
  --port 17670 \
  --drivers lxd \
  --compute-driver-socket /tmp/openshell-driver.sock \
  --disable-tls \
  --config "$REPO/gateway-dev.toml" \
  --log-level info &
```

Expected: `INFO TLS disabled — accepting plaintext connections`

`gateway-dev.toml` sets `allow_unauthenticated_users = true` so the CLI and
grpcurl don't need a bearer token. It is local-dev only.

---

## 4. Create a sandbox

Via the CLI:

```bash
export PATH="/home/kadinsayani/git/lxd-dev/git/OpenShell/target/release:$PATH"
openshell gateway add http://127.0.0.1:17670 --name lxd-local
openshell gateway select lxd-local
openshell sandbox create --name demo1
```

Or via grpcurl directly:

```bash
PROTO=/home/kadinsayani/git/lxd-dev/git/OpenShell/proto
grpcurl -plaintext \
  -proto "$PROTO/openshell.proto" -import-path "$PROTO" \
  -d '{"name":"demo1","spec":{"template":{"image":"ignored"},"policy":{"version":1,"filesystem":{"include_workdir":true}}}}' \
  localhost:17670 openshell.v1.OpenShell/CreateSandbox
```

Expected: response with `"phase": "SANDBOX_PHASE_PROVISIONING"`.

---

## 5. Verify the sandbox is ready

```bash
# Via CLI
openshell sandbox list

# Via grpcurl
PROTO=/home/kadinsayani/git/lxd-dev/git/OpenShell/proto
grpcurl -plaintext \
  -proto "$PROTO/openshell.proto" -import-path "$PROTO" \
  localhost:17670 openshell.v1.OpenShell/ListSandboxes
```

Wait ~5–10 seconds for `"phase": "SANDBOX_PHASE_READY"` and `"status": "True"` on the Ready condition.

Cross-check against LXD:

```bash
lxc list demo1
lxc exec demo1 -- ps aux
```

Expected from `ps aux`:
- PID 1 (root): supervisor (`openshell-sandbox`)
- PID ~60 (sandbox user, uid 10001): agent process spawned by supervisor

Check the supervisor connected to the gateway:

```bash
sudo cat /var/snap/lxd/common/lxd/logs/demo1/console.log
```

Expected tail:
```
[container-init] eth0 acquired IPv4: 10.120.210.x/24
[container-init] seeded /etc/hosts: host.openshell.internal → 10.120.210.1
[container-init] OPENSHELL_ENDPOINT probe: reachable (http://10.120.210.1:17670)
[container-init] exec: ... /opt/openshell/bin/openshell-sandbox --workdir /sandbox --ssh-socket-path /tmp/openshell-relay.sock
WARN runtime cgroup pids.max is unlimited ...   (expected, non-fatal)
OCSF SSH:LISTEN [INFO]
OCSF LIFECYCLE:INSTALL [INFO] OpenShell Sandbox Supervisor success
```
Then the supervisor goes quiet. The gateway log will show `GetSandboxConfig → 200`
confirming policy was fetched and the SSH relay socket is ready.

---

## 6. Open the TUI

```bash
openshell term
```

The dashboard shows `demo1` with live status. `q` to quit.

---

## 7. Clean up

```bash
openshell sandbox delete demo1

# Or skip the gateway and delete directly from LXD:
lxc delete demo1 --force

# Stop driver and gateway:
pkill -f "openshell-driver-lxd --socket"
pkill -f "openshell-gateway"
rm -f /tmp/openshell-driver.sock
```

---

## Driver-only verification (no CLI needed)

These commands talk to the driver directly over its Unix socket — no gateway,
no auth required.

```bash
PROTO=/home/kadinsayani/git/lxd-dev/git/openshell-driver-lxd/proto/compute_driver.proto
SOCK=unix:///tmp/openshell-driver.sock

grpcurl -plaintext -proto "$PROTO" -import-path "$(dirname "$PROTO")" \
  "$SOCK" openshell.compute.v1.ComputeDriver/GetCapabilities

grpcurl -plaintext -proto "$PROTO" -import-path "$(dirname "$PROTO")" \
  -d '{"sandbox":{"id":"sb-1","name":"sb-1","namespace":"default","spec":{"template":{"image":"ignored"}}}}' \
  "$SOCK" openshell.compute.v1.ComputeDriver/CreateSandbox

grpcurl -plaintext -proto "$PROTO" -import-path "$(dirname "$PROTO")" \
  -d '{"sandbox_name":"sb-1"}' "$SOCK" openshell.compute.v1.ComputeDriver/GetSandbox

grpcurl -plaintext -proto "$PROTO" -import-path "$(dirname "$PROTO")" \
  -d '{"sandbox_name":"sb-1"}' "$SOCK" openshell.compute.v1.ComputeDriver/DeleteSandbox
```

Note: driver-direct creates won't have a gateway-minted sandbox token, so the
supervisor inside the container will fail to authenticate and retry policy fetch
indefinitely. The container will still run — it just won't reach READY phase.
Use the gateway path (steps 3–5) for full end-to-end testing.
