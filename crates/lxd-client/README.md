# lxd-client

Async HTTP client for the [LXD](https://github.com/canonical/lxd) REST API,
supporting both a local Unix domain socket and a remote HTTPS+mTLS endpoint.

## Transport

| Endpoint | When to use |
|---|---|
| `LxdEndpoint::UnixSocket(path)` | Local LXD snap (`/var/snap/lxd/common/lxd/unix.socket`) |
| `LxdEndpoint::Https(LxdHttpsConfig)` | Remote LXD cluster over HTTPS with mutual TLS |

Both transports use HTTP/1.1 with a fresh connection per request.

## Usage

```rust
use std::path::PathBuf;
use lxd_client::{LxdClient, LxdEndpoint, LxdHttpsConfig};

// Unix socket (local LXD)
let lxd = LxdClient::new(LxdEndpoint::UnixSocket(
    PathBuf::from("/var/snap/lxd/common/lxd/unix.socket"),
)).unwrap();

// HTTPS+mTLS (remote LXD)
let lxd = LxdClient::new(LxdEndpoint::Https(LxdHttpsConfig {
    url: "https://10.0.0.1:8443".to_string(),
    client_cert: PathBuf::from("/etc/lxd/client.crt"),
    client_key: PathBuf::from("/etc/lxd/client.key"),
    server_ca: None, // use webpki CA bundle
}))?;
```

## API surface

**Instance lifecycle** (`instances.rs`)

- `create_instance(name, image_alias, config, devices, profiles, start)`
- `get_instance(name)` / `get_instance_state(name)` / `list_instances()`
- `start_instance(name)` / `stop_instance(name, force)`
- `delete_instance(name)`
- `push_file_into_instance(name, guest_path, content)` — writes directly into the container overlay; container need not be running

**Operation waiting** (`operations.rs`)

- `get_operation(id)` — non-blocking fetch of current operation state
- `wait_operation(id)` — subscribes to `GET /1.0/events?type=operation` via WebSocket and waits for the terminal event; falls back to REST long-poll if WebSocket is unavailable; reconciles on reconnect to close the subscribe/check race

**Event streaming** (`events.rs`)

- `subscribe_events(types)` — opens a WebSocket to `GET /1.0/events?type=<types>` and returns a `Stream<Item = Result<LxdEvent, LxdError>>`

**Networks** (`networks.rs`)

- `get_network(name)`

**Network ACLs** (`acls.rs`)

- `ensure_network_acl(name, egress)` — idempotent create-or-replace
- `delete_network_acl(name)` — 404 treated as success

**Resource quantity conversion** (`resources.rs`)

- `cpu_limit_to_lxd(quantity)` — converts Kubernetes-style CPU quantities to LXD `limits.cpu`
- `memory_limit_to_lxd(quantity)` — converts Kubernetes-style memory quantities to LXD `limits.memory`

## Error handling

All methods return `Result<_, LxdError>`. Key variants:

- `LxdError::Api { status_code, message }` — non-2xx HTTP response or LXD error envelope
- `LxdError::OperationFailed { description, err }` — async operation reached `Failure` state
- `LxdError::Tls { reason }` — certificate/key/URL errors while setting up TLS
- `LxdError::Io` / `LxdError::Hyper` — transport-level errors (connection refused, socket gone, TLS handshake failure)
