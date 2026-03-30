# gitea-ci-autoscaler

A Kubernetes-native autoscaler for [Gitea Actions](https://docs.gitea.com/usage/actions/overview) that automatically provisions and tears down [Hetzner Cloud](https://www.hetzner.com/cloud/) compute nodes based on CI job demand. It joins nodes to a [K3s](https://k3s.io/) cluster, monitors job queues, and removes idle nodes — optimizing for cost by respecting Hetzner's hourly billing windows.

## How It Works

The autoscaler runs a continuous reconciliation loop (default: every 5 seconds):

1. **Query state** — Fetches waiting/running jobs from Gitea, lists managed Hetzner servers, K8s nodes, and runner pods.
2. **Reconcile** — Matches servers to nodes to runners and classifies each as Provisioning, Busy, Idle, Draining, or Removing.
3. **Scale up** — If there are more waiting jobs than available capacity (idle + provisioning + permanent runners), creates new Hetzner servers that auto-join the K3s cluster via cloud-init.
4. **Scale down** — Idle nodes past the timeout are torn down in stages: deregister runner from Gitea, drain the K8s node, delete the node, delete the Hetzner server. Teardown is deferred to the end of the billing hour to avoid paying for unused time.
5. **Clean up** — Servers stuck in provisioning beyond a timeout are automatically deleted.

### Node Lifecycle

```
[Create Server] → Provisioning → Busy → Idle → Deregistering → Draining → Removing → [Deleted]
```

## Configuration

All configuration is via environment variables.

| Variable | Default | Description |
|---|---|---|
| `HETZNER_API_TOKEN` | *required* | Hetzner Cloud API token |
| `GITEA_ADMIN_TOKEN` | *required* | Gitea admin API token |
| `CLUSTER_SECRET` | *required* | K3s join token for new nodes |
| `GITEA_API_URL` | *required* | Gitea API endpoint |
| `RUNNER_NAMESPACE` | `gitea-runners` | Kubernetes namespace for runner pods |
| `MAX_NODES` | `5` | Maximum number of managed nodes |
| `POLL_INTERVAL_SECS` | `5` | Reconciliation loop frequency (seconds) |
| `IDLE_TIMEOUT_MINS` | `5` | How long a node must be idle before teardown |
| `BILLING_WINDOW_MINS` | `5` | Only tear down nodes in the last N minutes of their billing hour |
| `PROVISIONING_TIMEOUT_SECS` | `600` | Max time to wait for a node to become ready |
| `HETZNER_SERVER_TYPE` | `ccx33` | Hetzner instance type |
| `HETZNER_LOCATION` | `nbg1` | Hetzner datacenter location |
| `HETZNER_IMAGE` | `ubuntu-24.04` | OS image for new servers |
| `PUSHGATEWAY_URL` | *required* | Prometheus Pushgateway endpoint |

## Metrics

Pushes Prometheus metrics to the configured Pushgateway, including:

- Node counts by state (provisioning, busy, idle, draining, removing)
- Waiting and running job counts
- Nodes created/deleted totals
- Scale up/down error counts
- API latency histograms (Hetzner, Gitea, K8s)
- Per-node age and idle duration
- Provisioning duration (server creation to ready)
- Control loop duration

## Building

```bash
# Run tests and checks
cargo test
cargo clippy

# Build release binary
cargo build --release

# Build and push Docker image
docker buildx build --platform linux/amd64 -t ghcr.io/rustunit/gitea-ci-autoscaler:latest --push .
```

The Docker image is built as a static musl binary and runs from `scratch` — the final image contains only the binary and CA certificates.

## Prerequisites

### Cluster

- A **K3s cluster** — the autoscaler uses K3s-specific mechanisms to join new nodes (the `get.k3s.io` install script, K3s join token, and K3s version detection from existing nodes). The control plane must be reachable from newly created Hetzner servers on port 6443.
- A **Hetzner Cloud** project with an API token and at least one SSH key — all project SSH keys are automatically added to created servers.
- A **Gitea** instance with Actions enabled and an admin API token.

> **Note:** The core design (reconcile loop, node lifecycle, Hetzner provisioning) is not fundamentally tied to K3s and could be adapted to work with standard Kubernetes or other distributions. Contributions to make this more flexible are welcome — see [Contributing](#contributing) below.

### Runner Pods

The autoscaler expects Gitea Actions runner pods to already be deployed in `RUNNER_NAMESPACE` (default: `gitea-runners`). These pods must:

- Have the label **`app=gitea-actions-runner`** — this is how the autoscaler discovers runners and maps them to nodes.
- Be scheduled onto the managed nodes (e.g. via a DaemonSet or Deployment with a `nodeSelector` for `node-role=ci-runner`).

New Hetzner servers join K3s with the labels `managed-by=gitea-ci-autoscaler` and `node-role=ci-runner`, so your runner workloads should target those labels to land on autoscaled nodes.

### Namespace

The `RUNNER_NAMESPACE` (default: `gitea-runners`) must exist before starting the autoscaler. The autoscaler itself can run in any namespace but needs a `ClusterRole` since nodes are cluster-scoped resources. The required RBAC permissions are narrowly scoped:

```yaml
# ClusterRole
rules:
  - apiGroups: [""]
    resources: [nodes]
    verbs: [list, patch, delete]
  - apiGroups: [""]
    resources: [pods]
    verbs: [list, get, create]     # create is needed for eviction
  - apiGroups: [""]
    resources: [pods/eviction]
    verbs: [create]
```

Additionally, a namespaced `Role` in `RUNNER_NAMESPACE` is needed to list runner pods:

```yaml
# Role in RUNNER_NAMESPACE
rules:
  - apiGroups: [""]
    resources: [pods]
    verbs: [list]
```

## Deployment

Designed to run as a single pod inside the K3s cluster it manages. It needs:

- In-cluster Kubernetes access (via a service account with the RBAC permissions above)
- Network access to the Gitea API and Hetzner Cloud API
- The K3s join token (`CLUSTER_SECRET`) so new nodes can register with the cluster

> **Recommendation:** Because the autoscaler holds broad node-level permissions (drain, delete) and a Hetzner API token that can create/destroy servers, we strongly recommend running it in a **dedicated K3s cluster** separate from your production workloads. This limits the blast radius if the service account or token is compromised and keeps CI node churn isolated from your application infrastructure.

## Contributing

Contributions are welcome! Some areas where help would be appreciated:

- Support for standard Kubernetes (kubeadm) or other distributions beyond K3s
- Support for additional cloud providers beyond Hetzner
- Helm chart or Kustomize manifests for easier deployment

Feel free to open an issue or pull request on [GitHub](https://github.com/rustunit/gitea-ci-autoscaler).

## License

MIT
