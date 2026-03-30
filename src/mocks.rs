use async_trait::async_trait;
use chrono::Utc;
use std::sync::Mutex;

use crate::gitea::{GiteaClient, Job, JobStatus, Runner};
use crate::hetzner::{HetznerClient, HetznerServer};
use crate::k8s::{K8sNode, K8sPod, KubeClient};

// --- Mock Gitea Client ---

pub struct MockGiteaClient {
    pub jobs: Mutex<Vec<Job>>,
    pub runners: Mutex<Vec<Runner>>,
    pub delete_runner_calls: Mutex<Vec<u64>>,
    pub fail_list_jobs: Mutex<bool>,
    pub fail_list_runners: Mutex<bool>,
}

impl MockGiteaClient {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            runners: Mutex::new(Vec::new()),
            delete_runner_calls: Mutex::new(Vec::new()),
            fail_list_jobs: Mutex::new(false),
            fail_list_runners: Mutex::new(false),
        }
    }
}

#[async_trait]
impl GiteaClient for MockGiteaClient {
    async fn list_jobs(&self, status: JobStatus) -> anyhow::Result<Vec<Job>> {
        if *self.fail_list_jobs.lock().unwrap() {
            return Err(anyhow::anyhow!("mock: Gitea API error"));
        }
        let status_str = status.as_str();
        let jobs = self.jobs.lock().unwrap();
        Ok(jobs
            .iter()
            .filter(|j| j.status == status_str)
            .cloned()
            .collect())
    }

    async fn list_runners(&self) -> anyhow::Result<Vec<Runner>> {
        if *self.fail_list_runners.lock().unwrap() {
            return Err(anyhow::anyhow!("mock: Gitea API error"));
        }
        Ok(self.runners.lock().unwrap().clone())
    }

    async fn delete_runner(&self, runner_id: u64) -> anyhow::Result<()> {
        self.delete_runner_calls.lock().unwrap().push(runner_id);
        Ok(())
    }
}

// --- Mock Hetzner Client ---

pub struct MockHetznerClient {
    pub servers: Mutex<Vec<HetznerServer>>,
    pub create_calls: Mutex<Vec<(String, String)>>,
    pub delete_calls: Mutex<Vec<i64>>,
    pub fail_create: Mutex<bool>,
    pub fail_delete: Mutex<bool>,
    next_server_id: Mutex<i64>,
}

impl MockHetznerClient {
    pub fn new() -> Self {
        Self {
            servers: Mutex::new(Vec::new()),
            create_calls: Mutex::new(Vec::new()),
            delete_calls: Mutex::new(Vec::new()),
            fail_create: Mutex::new(false),
            fail_delete: Mutex::new(false),
            next_server_id: Mutex::new(1000),
        }
    }
}

#[async_trait]
impl HetznerClient for MockHetznerClient {
    async fn create_server(&self, name: &str, cloud_init: &str) -> anyhow::Result<HetznerServer> {
        if *self.fail_create.lock().unwrap() {
            return Err(anyhow::anyhow!("mock: Hetzner create failed"));
        }
        self.create_calls
            .lock()
            .unwrap()
            .push((name.to_string(), cloud_init.to_string()));

        let mut id = self.next_server_id.lock().unwrap();
        let server = HetznerServer {
            id: *id,
            name: name.to_string(),
            created: Utc::now(),
            labels: {
                let mut m = std::collections::HashMap::new();
                m.insert("managed-by".to_string(), "gitea-ci-autoscaler".to_string());
                m
            },
        };
        *id += 1;
        self.servers.lock().unwrap().push(server.clone());
        Ok(server)
    }

    async fn list_servers(&self) -> anyhow::Result<Vec<HetznerServer>> {
        Ok(self.servers.lock().unwrap().clone())
    }

    async fn delete_server(&self, server_id: i64) -> anyhow::Result<()> {
        if *self.fail_delete.lock().unwrap() {
            return Err(anyhow::anyhow!("mock: Hetzner delete failed"));
        }
        self.delete_calls.lock().unwrap().push(server_id);
        self.servers.lock().unwrap().retain(|s| s.id != server_id);
        Ok(())
    }
}

// --- Mock Kube Client ---

pub struct MockKubeClient {
    pub nodes: Mutex<Vec<K8sNode>>,
    pub pods: Mutex<Vec<K8sPod>>,
    pub drain_calls: Mutex<Vec<String>>,
    pub delete_node_calls: Mutex<Vec<String>>,
    pub k3s_version: Mutex<String>,
    pub master_ip: Mutex<String>,
}

impl MockKubeClient {
    pub fn new() -> Self {
        Self {
            nodes: Mutex::new(Vec::new()),
            pods: Mutex::new(Vec::new()),
            drain_calls: Mutex::new(Vec::new()),
            delete_node_calls: Mutex::new(Vec::new()),
            k3s_version: Mutex::new("v1.32.11+k3s1".to_string()),
            master_ip: Mutex::new("10.0.0.1".to_string()),
        }
    }
}

#[async_trait]
impl KubeClient for MockKubeClient {
    async fn list_nodes(&self, _label_selector: &str) -> anyhow::Result<Vec<K8sNode>> {
        Ok(self.nodes.lock().unwrap().clone())
    }

    async fn list_pods(
        &self,
        _namespace: &str,
        _label_selector: &str,
    ) -> anyhow::Result<Vec<K8sPod>> {
        Ok(self.pods.lock().unwrap().clone())
    }

    async fn drain_node(&self, node_name: &str) -> anyhow::Result<()> {
        self.drain_calls.lock().unwrap().push(node_name.to_string());
        Ok(())
    }

    async fn delete_node(&self, node_name: &str) -> anyhow::Result<()> {
        self.delete_node_calls
            .lock()
            .unwrap()
            .push(node_name.to_string());
        Ok(())
    }

    async fn get_k3s_version(&self) -> anyhow::Result<String> {
        Ok(self.k3s_version.lock().unwrap().clone())
    }

    async fn get_master_ip(&self) -> anyhow::Result<String> {
        Ok(self.master_ip.lock().unwrap().clone())
    }
}
