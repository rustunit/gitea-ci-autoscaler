use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct K8sNode {
    pub name: String,
    pub unschedulable: bool,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct K8sPod {
    pub name: String,
    pub namespace: String,
    pub node_name: Option<String>,
}

#[async_trait]
pub trait KubeClient: Send + Sync {
    async fn list_nodes(&self, label_selector: &str) -> anyhow::Result<Vec<K8sNode>>;
    async fn list_pods(&self, namespace: &str, label_selector: &str)
    -> anyhow::Result<Vec<K8sPod>>;
    async fn drain_node(&self, node_name: &str) -> anyhow::Result<()>;
    async fn delete_node(&self, node_name: &str) -> anyhow::Result<()>;
    async fn get_k3s_version(&self) -> anyhow::Result<String>;
    async fn get_master_ip(&self) -> anyhow::Result<String>;
}

// --- Real implementation ---

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams};
use tracing::info;

pub struct RealKubeClient {
    client: kube::Client,
}

impl RealKubeClient {
    pub async fn new() -> anyhow::Result<Self> {
        let client = kube::Client::try_default().await?;
        Ok(Self { client })
    }
}

#[async_trait]
impl KubeClient for RealKubeClient {
    async fn list_nodes(&self, label_selector: &str) -> anyhow::Result<Vec<K8sNode>> {
        let nodes_api: Api<Node> = Api::all(self.client.clone());
        let lp = ListParams::default().labels(label_selector);
        let nodes = nodes_api.list(&lp).await?;

        let result: Vec<_> = nodes
            .items
            .into_iter()
            .map(|n| K8sNode {
                name: n.metadata.name.unwrap_or_default(),
                unschedulable: n.spec.and_then(|s| s.unschedulable).unwrap_or(false),
            })
            .collect();
        info!(count = result.len(), label_selector, "listed k8s nodes");
        Ok(result)
    }

    async fn list_pods(
        &self,
        namespace: &str,
        label_selector: &str,
    ) -> anyhow::Result<Vec<K8sPod>> {
        let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let lp = ListParams::default().labels(label_selector);
        let pods = pods_api.list(&lp).await?;

        let result: Vec<_> = pods
            .items
            .into_iter()
            .map(|p| K8sPod {
                name: p.metadata.name.unwrap_or_default(),
                namespace: p.metadata.namespace.unwrap_or_default(),
                node_name: p.spec.and_then(|s| s.node_name),
            })
            .collect();
        info!(
            count = result.len(),
            namespace, label_selector, "listed k8s pods"
        );
        Ok(result)
    }

    async fn drain_node(&self, node_name: &str) -> anyhow::Result<()> {
        let nodes_api: Api<Node> = Api::all(self.client.clone());

        // Cordon: mark node as unschedulable
        let patch = serde_json::json!({
            "spec": { "unschedulable": true }
        });
        nodes_api
            .patch(
                node_name,
                &kube::api::PatchParams::default(),
                &kube::api::Patch::Merge(&patch),
            )
            .await?;

        // Evict all pods on this node
        let pods_api: Api<Pod> = Api::all(self.client.clone());
        let lp = ListParams::default().fields(&format!("spec.nodeName={node_name}"));
        let pods = pods_api.list(&lp).await?;

        for pod in pods.items {
            let pod_name = pod.metadata.name.as_deref().unwrap_or_default();
            let pod_ns = pod.metadata.namespace.as_deref().unwrap_or("default");

            // Skip mirror pods (managed by kubelet)
            if pod
                .metadata
                .annotations
                .as_ref()
                .is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"))
            {
                continue;
            }

            // Skip DaemonSet pods
            if pod
                .metadata
                .owner_references
                .as_ref()
                .is_some_and(|refs| refs.iter().any(|r| r.kind == "DaemonSet"))
            {
                continue;
            }

            let eviction_api: Api<Pod> = Api::namespaced(self.client.clone(), pod_ns);
            let _ = eviction_api
                .evict(pod_name, &kube::api::EvictParams::default())
                .await;
        }

        info!(node_name, "drained k8s node");
        Ok(())
    }

    async fn delete_node(&self, node_name: &str) -> anyhow::Result<()> {
        let nodes_api: Api<Node> = Api::all(self.client.clone());
        nodes_api
            .delete(node_name, &kube::api::DeleteParams::default())
            .await?;
        info!(node_name, "deleted k8s node");
        Ok(())
    }

    async fn get_k3s_version(&self) -> anyhow::Result<String> {
        let version = self.client.apiserver_version().await?;
        Ok(version.git_version)
    }

    async fn get_master_ip(&self) -> anyhow::Result<String> {
        let nodes_api: Api<Node> = Api::all(self.client.clone());
        let lp = ListParams::default().labels("node-role.kubernetes.io/control-plane");
        let nodes = nodes_api.list(&lp).await?;

        let master = nodes
            .items
            .first()
            .ok_or_else(|| anyhow::anyhow!("no control-plane node found"))?;

        let addresses = master
            .status
            .as_ref()
            .and_then(|s| s.addresses.as_ref())
            .ok_or_else(|| anyhow::anyhow!("control-plane node has no addresses"))?;

        addresses
            .iter()
            .find(|a| a.type_ == "InternalIP")
            .map(|a| a.address.clone())
            .ok_or_else(|| anyhow::anyhow!("control-plane node has no InternalIP"))
    }
}
