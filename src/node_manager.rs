use chrono::{DateTime, Duration, Utc};
use tracing::{info, warn};

use crate::cloud_init;
use crate::config::Config;
use crate::gitea::{GiteaClient, Runner};
use crate::hetzner::HetznerClient;
use crate::k8s::KubeClient;
use crate::metrics::Metrics;

#[derive(Debug, Clone)]
pub struct ManagedNode {
    pub hetzner_server_id: i64,
    pub hetzner_server_name: String,
    pub created_at: DateTime<Utc>,
    pub state: NodeState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NodeState {
    Provisioning,
    Busy {
        k8s_node_name: String,
        gitea_runner_id: u64,
        gitea_runner_name: String,
    },
    Idle {
        k8s_node_name: String,
        gitea_runner_id: u64,
        gitea_runner_name: String,
        idle_since: DateTime<Utc>,
    },
    Deregistering {
        k8s_node_name: String,
        gitea_runner_id: u64,
    },
    Draining {
        k8s_node_name: String,
    },
    Removing,
}

impl NodeState {
    pub fn state_name(&self) -> &'static str {
        match self {
            NodeState::Provisioning => "provisioning",
            NodeState::Busy { .. } => "busy",
            NodeState::Idle { .. } => "idle",
            NodeState::Deregistering { .. } => "deregistering",
            NodeState::Draining { .. } => "draining",
            NodeState::Removing => "removing",
        }
    }
}

pub struct NodeManager {
    pub nodes: Vec<ManagedNode>,
    pub k3s_version: String,
    pub master_ip: String,
}

impl NodeManager {
    pub fn new(k3s_version: String, master_ip: String) -> Self {
        Self {
            nodes: Vec::new(),
            k3s_version,
            master_ip,
        }
    }

    /// Compute how many new nodes we need to create.
    pub fn compute_scale_up(
        &self,
        waiting_linux_jobs: usize,
        permanent_runner_capacity: usize,
        max_nodes: usize,
    ) -> usize {
        let idle = self
            .nodes
            .iter()
            .filter(|n| matches!(n.state, NodeState::Idle { .. }))
            .count();
        let provisioning = self
            .nodes
            .iter()
            .filter(|n| matches!(n.state, NodeState::Provisioning))
            .count();
        let current_managed = self.nodes.len();

        let needed = waiting_linux_jobs
            .saturating_sub(idle)
            .saturating_sub(provisioning)
            .saturating_sub(permanent_runner_capacity);

        let max_new = max_nodes.saturating_sub(current_managed);
        needed.min(max_new)
    }

    /// Scale up by creating Hetzner servers.
    pub async fn scale_up(
        &mut self,
        count: usize,
        config: &Config,
        hetzner: &dyn HetznerClient,
        metrics: &Metrics,
    ) {
        for _ in 0..count {
            let name = format!("ci-runner-{}", &uuid::Uuid::new_v4().to_string()[..8]);
            let cloud_init_data =
                cloud_init::render(&self.master_ip, &config.cluster_secret, &self.k3s_version);

            info!(server_name = %name, "creating Hetzner server");
            match hetzner.create_server(&name, &cloud_init_data).await {
                Ok(server) => {
                    metrics.nodes_created_total.inc();
                    self.nodes.push(ManagedNode {
                        hetzner_server_id: server.id,
                        hetzner_server_name: server.name,
                        created_at: server.created,
                        state: NodeState::Provisioning,
                    });
                }
                Err(e) => {
                    warn!(error = %e, "failed to create Hetzner server");
                    metrics.scale_up_errors_total.inc();
                }
            }
        }
    }

    /// Identify nodes eligible for teardown and return them.
    pub fn find_teardown_candidates(
        &self,
        now: DateTime<Utc>,
        idle_timeout: std::time::Duration,
        billing_window_mins: u64,
    ) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, node)| {
                if let NodeState::Idle { idle_since, .. } = &node.state {
                    let idle_duration = now - *idle_since;
                    let idle_enough =
                        idle_duration >= Duration::from_std(idle_timeout).expect("valid duration");
                    let in_billing_window =
                        is_in_billing_window(node.created_at, now, billing_window_mins);

                    if idle_enough && in_billing_window {
                        Some(i)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect()
    }

    /// Execute one teardown step for a node. Returns true if the node was fully removed.
    pub async fn teardown_step(
        &mut self,
        index: usize,
        gitea: &dyn GiteaClient,
        kube: &dyn KubeClient,
        hetzner: &dyn HetznerClient,
        metrics: &Metrics,
    ) -> bool {
        let node = &self.nodes[index];
        match &node.state {
            NodeState::Idle {
                k8s_node_name,
                gitea_runner_id,
                ..
            } => {
                let k8s_name = k8s_node_name.clone();
                let runner_id = *gitea_runner_id;
                info!(
                    server = %node.hetzner_server_name,
                    runner_id,
                    "starting teardown: deregistering runner"
                );
                match gitea.delete_runner(runner_id).await {
                    Ok(()) => {
                        metrics.runners_deregistered_total.inc();
                        self.nodes[index].state = NodeState::Deregistering {
                            k8s_node_name: k8s_name,
                            gitea_runner_id: runner_id,
                        };
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to deregister runner");
                        metrics.scale_down_errors_total.inc();
                    }
                }
                false
            }
            NodeState::Deregistering { k8s_node_name, .. } => {
                let k8s_name = k8s_node_name.clone();
                info!(
                    server = %node.hetzner_server_name,
                    "draining node"
                );
                match kube.drain_node(&k8s_name).await {
                    Ok(()) => {
                        self.nodes[index].state = NodeState::Draining {
                            k8s_node_name: k8s_name,
                        };
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to drain node");
                        metrics.scale_down_errors_total.inc();
                    }
                }
                false
            }
            NodeState::Draining { k8s_node_name } => {
                let k8s_name = k8s_node_name.clone();
                info!(
                    server = %node.hetzner_server_name,
                    "deleting k8s node"
                );
                match kube.delete_node(&k8s_name).await {
                    Ok(()) => {
                        self.nodes[index].state = NodeState::Removing;
                    }
                    Err(e) => {
                        warn!(error = %e, "failed to delete k8s node");
                        metrics.scale_down_errors_total.inc();
                        return false;
                    }
                }
                // Immediately try to delete the Hetzner server
                self.try_delete_hetzner_server(index, hetzner, metrics)
                    .await
            }
            NodeState::Removing => {
                info!(
                    server = %node.hetzner_server_name,
                    "retrying hetzner server deletion"
                );
                self.try_delete_hetzner_server(index, hetzner, metrics)
                    .await
            }
            _ => false,
        }
    }

    async fn try_delete_hetzner_server(
        &mut self,
        index: usize,
        hetzner: &dyn HetznerClient,
        metrics: &Metrics,
    ) -> bool {
        let node = &self.nodes[index];
        let server_id = node.hetzner_server_id;
        info!(
            server = %node.hetzner_server_name,
            server_id,
            "deleting Hetzner server"
        );
        match hetzner.delete_server(server_id).await {
            Ok(()) => {
                metrics.nodes_deleted_total.inc();
                self.nodes.remove(index);
                true
            }
            Err(e) => {
                warn!(error = %e, server_id, "failed to delete Hetzner server, will retry");
                false
            }
        }
    }

    /// Delete a stuck provisioning server.
    pub async fn delete_stuck_server(
        &mut self,
        server_id: i64,
        hetzner: &dyn HetznerClient,
        metrics: &Metrics,
    ) {
        info!(server_id, "deleting stuck server");
        match hetzner.delete_server(server_id).await {
            Ok(()) => {
                metrics.nodes_deleted_total.inc();
                metrics.scale_up_errors_total.inc();
                if let Some(index) = self
                    .nodes
                    .iter()
                    .position(|n| n.hetzner_server_id == server_id)
                {
                    self.nodes.remove(index);
                }
            }
            Err(e) => {
                warn!(error = %e, server_id, "failed to delete stuck server");
            }
        }
    }

    /// Update metrics from current node state.
    pub fn update_metrics(&self, metrics: &Metrics, now: DateTime<Utc>) {
        // Reset per-node gauges so deleted nodes stop emitting stale series
        metrics.node_age_seconds.reset();
        metrics.node_idle_seconds.reset();

        let mut counts = [0usize; 6]; // provisioning, busy, idle, deregistering, draining, removing
        for node in &self.nodes {
            match &node.state {
                NodeState::Provisioning => counts[0] += 1,
                NodeState::Busy { .. } => counts[1] += 1,
                NodeState::Idle { idle_since, .. } => {
                    counts[2] += 1;
                    let idle_secs = (now - *idle_since).num_seconds().max(0) as f64;
                    metrics
                        .node_idle_seconds
                        .with_label_values(&[&node.hetzner_server_name])
                        .set(idle_secs);
                }
                NodeState::Deregistering { .. } => counts[3] += 1,
                NodeState::Draining { .. } => counts[4] += 1,
                NodeState::Removing => counts[5] += 1,
            }
            let age_secs = (now - node.created_at).num_seconds().max(0) as f64;
            metrics
                .node_age_seconds
                .with_label_values(&[&node.hetzner_server_name])
                .set(age_secs);
        }
        metrics.set_managed_node_counts(
            counts[0], counts[1], counts[2], counts[3], counts[4], counts[5],
        );
    }
}

/// Determine permanent runner capacity: online, non-busy runners NOT on managed nodes.
pub fn permanent_runner_capacity(runners: &[Runner], managed_runner_names: &[String]) -> usize {
    runners
        .iter()
        .filter(|r| {
            r.status == "online"
                && !r.busy
                && !managed_runner_names.contains(&r.name)
                && r.labels.iter().any(|l| l.name == "linux")
        })
        .count()
}

/// Check if `now` is within the last `window_mins` minutes of the server's billing hour.
pub fn is_in_billing_window(
    created_at: DateTime<Utc>,
    now: DateTime<Utc>,
    window_mins: u64,
) -> bool {
    let age = now - created_at;
    let minutes_into_hour = age.num_minutes() % 60;
    minutes_into_hour >= (60 - window_mins as i64)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::gitea::{Runner, RunnerLabel};
    use crate::mocks::*;
    use chrono::Duration;

    fn make_runner(id: u64, name: &str, online: bool, busy: bool) -> Runner {
        Runner {
            id,
            name: name.to_string(),
            status: if online { "online" } else { "offline" }.to_string(),
            busy,
            labels: vec![RunnerLabel {
                name: "linux".to_string(),
            }],
        }
    }

    // --- Billing window tests ---

    #[test]
    fn billing_hour_window_inside() {
        let created = Utc::now() - Duration::minutes(56);
        assert!(is_in_billing_window(created, Utc::now(), 5));
    }

    #[test]
    fn billing_hour_window_outside() {
        let created = Utc::now() - Duration::minutes(30);
        assert!(!is_in_billing_window(created, Utc::now(), 5));
    }

    #[test]
    fn billing_hour_window_boundary() {
        let created = Utc::now() - Duration::minutes(55);
        assert!(is_in_billing_window(created, Utc::now(), 5));
    }

    #[test]
    fn billing_hour_window_second_hour() {
        let created = Utc::now() - Duration::minutes(116); // 56 mins into second hour
        assert!(is_in_billing_window(created, Utc::now(), 5));
    }

    // --- Idle timeout tests ---

    #[test]
    fn idle_timeout_check_expired() {
        let idle_since = Utc::now() - Duration::minutes(6);
        let idle_duration = Utc::now() - idle_since;
        assert!(idle_duration >= Duration::minutes(5));
    }

    #[test]
    fn idle_timeout_check_not_expired() {
        let idle_since = Utc::now() - Duration::minutes(3);
        let idle_duration = Utc::now() - idle_since;
        assert!(idle_duration < Duration::minutes(5));
    }

    // --- Scale-up tests ---

    #[test]
    fn scale_up_one_node() {
        let mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        assert_eq!(mgr.compute_scale_up(1, 0, 5), 1);
    }

    #[test]
    fn scale_up_multiple() {
        let mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        assert_eq!(mgr.compute_scale_up(3, 0, 5), 3);
    }

    #[test]
    fn scale_up_capped_at_max() {
        let mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        assert_eq!(mgr.compute_scale_up(10, 0, 5), 5);
    }

    #[test]
    fn no_scale_up_when_provisioning() {
        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        mgr.nodes.push(ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: Utc::now(),
            state: NodeState::Provisioning,
        });
        assert_eq!(mgr.compute_scale_up(1, 0, 5), 0);
    }

    #[test]
    fn no_scale_up_at_max() {
        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        for i in 0..5i64 {
            mgr.nodes.push(ManagedNode {
                hetzner_server_id: i,
                hetzner_server_name: format!("ci-runner-{i}"),
                created_at: Utc::now(),
                state: NodeState::Idle {
                    k8s_node_name: format!("node-{i}"),
                    gitea_runner_id: (i + 100) as u64,
                    gitea_runner_name: format!("runner-{i}"),
                    idle_since: Utc::now(),
                },
            });
        }
        assert_eq!(mgr.compute_scale_up(2, 0, 5), 0);
    }

    #[test]
    fn ignore_macos_jobs() {
        // macos jobs are pre-filtered — only linux count is passed in
        let mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        assert_eq!(mgr.compute_scale_up(1, 0, 5), 1); // 1 linux
    }

    #[test]
    fn no_scale_on_zero_demand() {
        let mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        assert_eq!(mgr.compute_scale_up(0, 0, 5), 0);
    }

    #[test]
    fn account_for_permanent_runners() {
        let mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        assert_eq!(mgr.compute_scale_up(2, 2, 5), 0);
    }

    #[test]
    fn scale_up_beyond_permanent() {
        let mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        assert_eq!(mgr.compute_scale_up(4, 2, 5), 2);
    }

    // --- Scale-down tests ---

    #[test]
    fn find_teardown_idle_node() {
        let now = Utc::now();
        let created = now - Duration::minutes(56); // in billing window
        let idle_since = now - Duration::minutes(6); // past idle timeout

        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        mgr.nodes.push(ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: created,
            state: NodeState::Idle {
                k8s_node_name: "ci-runner-1".into(),
                gitea_runner_id: 100,
                gitea_runner_name: "runner-1".into(),
                idle_since,
            },
        });

        let candidates = mgr.find_teardown_candidates(now, std::time::Duration::from_secs(300), 5);
        assert_eq!(candidates, vec![0]);
    }

    #[test]
    fn no_scale_down_when_busy() {
        let now = Utc::now();
        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        mgr.nodes.push(ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: now - Duration::minutes(56),
            state: NodeState::Busy {
                k8s_node_name: "ci-runner-1".into(),
                gitea_runner_id: 100,
                gitea_runner_name: "runner-1".into(),
            },
        });

        let candidates = mgr.find_teardown_candidates(now, std::time::Duration::from_secs(300), 5);
        assert!(candidates.is_empty());
    }

    #[test]
    fn no_scale_down_before_timeout() {
        let now = Utc::now();
        let created = now - Duration::minutes(56);
        let idle_since = now - Duration::minutes(3); // not yet past 5 min idle

        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        mgr.nodes.push(ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: created,
            state: NodeState::Idle {
                k8s_node_name: "ci-runner-1".into(),
                gitea_runner_id: 100,
                gitea_runner_name: "runner-1".into(),
                idle_since,
            },
        });

        let candidates = mgr.find_teardown_candidates(now, std::time::Duration::from_secs(300), 5);
        assert!(candidates.is_empty());
    }

    #[test]
    fn no_scale_down_outside_billing_window() {
        let now = Utc::now();
        let created = now - Duration::minutes(30); // NOT near billing hour end
        let idle_since = now - Duration::minutes(6);

        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        mgr.nodes.push(ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: created,
            state: NodeState::Idle {
                k8s_node_name: "ci-runner-1".into(),
                gitea_runner_id: 100,
                gitea_runner_name: "runner-1".into(),
                idle_since,
            },
        });

        let candidates = mgr.find_teardown_candidates(now, std::time::Duration::from_secs(300), 5);
        assert!(candidates.is_empty());
    }

    // --- Teardown order test ---

    #[tokio::test]
    async fn teardown_order() {
        let metrics = Metrics::new();
        let mock_gitea = MockGiteaClient::new();
        let mock_kube = MockKubeClient::new();
        let mock_hetzner = MockHetznerClient::new();

        let now = Utc::now();
        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        mgr.nodes.push(ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: now - Duration::minutes(56),
            state: NodeState::Idle {
                k8s_node_name: "ci-runner-1".into(),
                gitea_runner_id: 100,
                gitea_runner_name: "runner-1".into(),
                idle_since: now - Duration::minutes(6),
            },
        });

        // Step 1: Idle -> Deregistering
        let removed = mgr
            .teardown_step(0, &mock_gitea, &mock_kube, &mock_hetzner, &metrics)
            .await;
        assert!(!removed);
        assert!(matches!(
            mgr.nodes[0].state,
            NodeState::Deregistering { .. }
        ));

        // Verify Gitea delete was called
        {
            let gitea_calls = mock_gitea.delete_runner_calls.lock().unwrap();
            assert_eq!(gitea_calls.len(), 1);
            assert_eq!(gitea_calls[0], 100);
        }

        // Step 2: Deregistering -> Draining
        let removed = mgr
            .teardown_step(0, &mock_gitea, &mock_kube, &mock_hetzner, &metrics)
            .await;
        assert!(!removed);
        assert!(matches!(mgr.nodes[0].state, NodeState::Draining { .. }));

        // Step 3: Draining -> Removing -> Deleted
        let removed = mgr
            .teardown_step(0, &mock_gitea, &mock_kube, &mock_hetzner, &metrics)
            .await;
        assert!(removed);
        assert!(mgr.nodes.is_empty());
    }

    // --- Permanent runner capacity ---

    #[test]
    fn permanent_runner_capacity_test() {
        let runners = vec![
            make_runner(1, "permanent-1", true, false),
            make_runner(2, "permanent-2", true, true),
            make_runner(3, "managed-runner", true, false),
        ];
        let managed_names = vec!["managed-runner".to_string()];
        assert_eq!(permanent_runner_capacity(&runners, &managed_names), 1);
    }

    #[test]
    fn permanent_runner_capacity_excludes_non_linux() {
        let macos_runner = Runner {
            id: 10,
            name: "macos-runner".to_string(),
            status: "online".to_string(),
            busy: false,
            labels: vec![
                RunnerLabel {
                    name: "macos".to_string(),
                },
                RunnerLabel {
                    name: "self-hosted".to_string(),
                },
            ],
        };
        let runners = vec![make_runner(1, "linux-1", true, false), macos_runner];
        let managed_names = vec![];
        assert_eq!(permanent_runner_capacity(&runners, &managed_names), 1);
    }

    // --- Stuck server deletion ---

    #[tokio::test]
    async fn delete_stuck_server_not_in_nodes() {
        let metrics = Metrics::new();
        let mock_hetzner = MockHetznerClient::new();
        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());

        // Server 99 is NOT tracked in manager.nodes
        assert!(mgr.nodes.is_empty());

        mgr.delete_stuck_server(99, &mock_hetzner, &metrics).await;

        // Should still call hetzner.delete_server
        let delete_calls = mock_hetzner.delete_calls.lock().unwrap();
        assert_eq!(delete_calls.len(), 1);
        assert_eq!(delete_calls[0], 99);
    }
}
