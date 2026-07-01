# MVP findings — openshell-driver-lxd ↔ openshell-gateway ↔ LXD

Working notes from getting the real `openshell-gateway` binary (built from
upstream `NVIDIA/OpenShell` main; `--compute-driver-socket` support landed via
PR #1703 which has since merged) talking to `openshell-driver-lxd` against a
real local LXD daemon. Everything below is uncommitted, sitting in the working
tree on `feat/openshell-sandbox-image`.

## What's verified working, end to end, with real binaries

- Gateway → driver handshake over the Unix socket (`GetCapabilities`, recognized
  as an out-of-tree/`in_tree=false` extension driver — the mechanism added by
  PR #1703).
- `CreateSandbox` through the gateway's public API provisions a real LXD
  container via our driver.
- The supervisor inside the container successfully authenticates to the
  gateway, fetches its policy (`GetSandboxConfig` → 200 confirmed in gateway
  logs), and spawns the agent process as the `sandbox` user (PID 62, uid 10001).
- The supervisor's own internal seccomp BPF filter and Landlock sandbox around
  the agent process install and run successfully.
- Full driver RPC lifecycle (Create/Get/List/Stop/Delete) against a real LXD
  daemon, both via direct grpcurl calls and via the real gateway.
- `lxc exec demo1 -- ps aux` shows PID 1 (supervisor, root) and PID 62
  (agent, sandbox user) running — the full container lifecycle is operational.

## Additional bugs found and fixed in follow-up sessions

5. **`OPENSHELL_SANDBOX` env var missing**: We injected `OPENSHELL_SANDBOX_ID`
   (the UUID, for gRPC policy fetch) but not `OPENSHELL_SANDBOX` (the sandbox
   name, for the supervisor's local policy-sync discovery path). The supervisor
   exited with "Cannot sync discovered policy: sandbox not available." Fixed by
   injecting `OPENSHELL_SANDBOX = sandbox.name` in `mapping.rs`.
6. **Nested seccomp blocked by LXD's default seccomp profile**: The supervisor
   installs its own BPF filter around the agent process. LXD's default container
   seccomp profile blocks `seccomp(2)` from inside unprivileged containers.
   `security.nesting=true` alone was insufficient (it enables nested namespaces,
   not nested seccomp). Fixed by also setting `security.syscalls.deny_default=false`
   on all sandbox containers — this removes LXD's syscall deny list while
   preserving full user-namespace isolation (not the same as privileged mode).
7. **Demo auth errors**: Even with `--disable-tls`, the gateway's JWT
   authenticator chain rejects all requests without a bearer token. Fixed by
   adding `gateway-dev.toml` with `allow_unauthenticated_users = true` and
   passing `--config gateway-dev.toml` in the gateway launch command.

## Real bugs found and fixed along the way

1. **`lxd-client`**: `InstanceState.network`/`.disk` are JSON `null` (not `{}`)
   once an instance is stopped — undetected deserialization bug, would have
   broken `ListSandboxes`/`GetSandbox` on any stopped instance in production.
   Fixed with a custom deserializer + regression test against real LXD.
2. **Vendored `compute_driver.proto` was stale**: missing `sandbox_token` and
   `driver_config` fields upstream added; still had `supports_gpu` which
   upstream removed. Resynced to upstream's current contract.
3. **Sandbox image had no DHCP client at all** (`rockcraft.yaml` stage-packages
   only had `ca-certificates curl iproute2 python3-minimal sqlite3`) — `eth0`
   never got an IPv4 lease, only a kernel SLAAC IPv6 link-local address. This
   was the actual root cause of "supervisor can't connect," not TLS or auth as
   I initially assumed and spent significant time chasing. Fixed by adding
   `isc-dhcp-client` and a `dhclient` step in the init wrapper.
4. **No `sandbox` user/group in the image** — the supervisor requires one by
   name. Fixed via a rockcraft `override-overlay` step (`useradd`/`groupadd`,
   UID/GID 10001, matching the init wrapper's existing convention).

## Open questions for you to weigh in on

### 1. Container security profile for sandboxes — RESOLVED

Both layers of the security profile problem are now fixed and the supervisor
runs end-to-end:

- **Network-namespace enforcement** (`--mode=process`): supervisor's netns/
  nftables mode requires real `CAP_SYS_ADMIN` in the host user namespace.
  Resolved by running the supervisor with `--mode=process` (skips network
  enforcement), appropriate for our driver which doesn't configure egress
  ACLs yet.
- **Nested seccomp BPF filter** (`security.syscalls.deny_default=false`): the
  supervisor installs its own BPF seccomp filter around the agent process.
  LXD's default container seccomp profile blocks `seccomp(2)` from inside
  unprivileged containers. `security.nesting=true` alone didn't help (it
  enables clone/unshare paths, not nested seccomp installation). Setting
  `security.syscalls.deny_default=false` removes LXD's syscall deny list for
  the sandbox container while preserving full user-namespace isolation.

Both settings (`security.nesting=true` + `security.syscalls.deny_default=false`)
are now injected by `mapping.rs::build_create_config` for every sandbox.

The remaining open design question is whether `security.syscalls.deny_default=false`
is the right long-term posture, or whether a narrower allowlist (specific
syscalls only) is worth the complexity. For the same reason we use `--mode=process`
rather than full network enforcement, this is a dev-environment default; a
production deployment would want to audit the exact syscalls the supervisor
requires and allow only those.

### 2. `OPENSHELL_SANDBOX_TOKEN` delivery: raw env var vs file
I used the raw `OPENSHELL_SANDBOX_TOKEN` env var, which the supervisor's own
docs say is "used only by test harnesses." Docker/Podman/VM drivers use
`OPENSHELL_SANDBOX_TOKEN_FILE` (a bind-mounted file) in production. LXD has no
direct bind-mount equivalent for instance creation; doing this properly means
adding a `push_file`-style method to `lxd-client` (a new `POST
/1.0/instances/{name}/files` call) and writing the token into the container's
rootfs around create time. Fine for an MVP; should be revisited before this
is anything but local-dev.

### 3. Gateway auth/TLS shape for a real deployment
For this test the gateway runs with `--disable-tls` + `gateway-dev.toml`
(`allow_unauthenticated_users = true`). These two flags are complementary:
`--disable-tls` is needed so the supervisor's bearer-JWT auth isn't blocked
by a client-cert mandate (`--tls-client-ca` forces `require_client_auth = true`
for *every* connection, including the supervisor's). The TOML config is needed
because even on a plaintext gateway the JWT authenticator rejects requests
without a bearer token by default — without it, all CLI/grpcurl calls hit
`Unauthenticated: missing authorization header`.

A real deployment needs TLS; the gateway's auth model supports bearer-JWT
(sandboxes) and mTLS (interactive users/CLI) concurrently as long as you don't
force client-cert verification on every connection. Worth deciding the real
TLS/cert story before this goes beyond local dev.

### 4. `lxd-client`'s `wait_operation` timeout design (separate PR thread)
Already discussed on PR #7: dropping the `timeout_secs` parameter from
`wait_operation` in favor of callers wrapping calls in `tokio::time::timeout`
(matching the `bollard`/`kube-rs` idiom) was the agreed direction, but **not
yet implemented**. Still open if you want it done before merging that PR.

### 5. CI for the sandbox image
`make sandbox-image` builds locally; CI (`.github/workflows/container-image.yaml`)
hasn't been touched to match the fixes above (DHCP client, sandbox user) or to
match the `--mode=process` init wrapper change. Deferred per your "worry about
CI later" instruction — flagging so it doesn't get lost.

### 6. Gateway binary provenance — RESOLVED (29 Jun 2026)
PR #1703 (`feat/external-compute-driver-socket`) merged into `NVIDIA/OpenShell`
main. The local clone at `/home/kadinsayani/git/lxd-dev/git/OpenShell` has been
re-pointed to upstream. The vendored proto was updated to pick up the
`ResourceRequirements` change (field 9 in `DriverSandboxSpec` changed from
`bool gpu` to `ResourceRequirements resource_requirements`); `driver.rs` was
updated accordingly. All tests pass against the upstream gateway binary.
