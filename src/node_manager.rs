use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use tracing::{debug, info, warn};

use crate::cloud_init;
use crate::config::Config;
use crate::gitea::{GiteaClient, Runner};
use crate::hetzner::{CreateError, HetznerClient, HetznerServer, Placement};
use crate::k8s::{K8sNode, K8sPod, KubeClient};
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

/// A new server can lag behind in `list_servers`; only after this is a missing one looked up.
const VANISH_GRACE_SECS: i64 = 20;

/// How long a node must be NotReady with no Hetzner server before it is removed.
/// Short enough to beat a one-minute NotReady alert.
const ORPHAN_GRACE_SECS: i64 = 30;

pub struct NodeManager {
    pub nodes: Vec<ManagedNode>,
    pub k3s_version: String,
    pub master_ip: String,
    /// Placements Hetzner had no capacity for, and when to try them again.
    unavailable_until: HashMap<Placement, DateTime<Utc>>,
    /// Where each server that has not joined yet was created, to blame if it vanishes.
    pending_placements: HashMap<i64, Placement>,
    /// K8s nodes seen NotReady without a Hetzner server, and since when.
    orphaned_since: HashMap<String, DateTime<Utc>>,
}

impl NodeManager {
    pub fn new(k3s_version: String, master_ip: String) -> Self {
        Self {
            nodes: Vec::new(),
            k3s_version,
            master_ip,
            unavailable_until: HashMap::new(),
            pending_placements: HashMap::new(),
            orphaned_since: HashMap::new(),
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
        now: DateTime<Utc>,
    ) {
        self.unavailable_until.retain(|_, until| *until > now);
        for _ in 0..count {
            if !self.create_server(config, hetzner, metrics, now).await {
                break;
            }
        }
    }

    /// Create one server in the first placement with capacity. False when none has any.
    async fn create_server(
        &mut self,
        config: &Config,
        hetzner: &dyn HetznerClient,
        metrics: &Metrics,
        now: DateTime<Utc>,
    ) -> bool {
        let name = format!("ci-runner-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let cloud_init_data = cloud_init::render(
            &self.master_ip,
            &config.cluster_secret,
            &self.k3s_version,
            &config.k3s_agent_args,
        );

        let mut tried = false;
        for placement in config.placements() {
            if self.unavailable_until.contains_key(&placement) {
                continue;
            }
            tried = true;

            info!(
                server_name = %name,
                server_type = %placement.server_type,
                location = %placement.location,
                "creating Hetzner server"
            );
            match hetzner
                .create_server(&name, &cloud_init_data, &placement)
                .await
            {
                Ok(server) => {
                    metrics.nodes_created_total.inc();
                    metrics
                        .nodes_created_by_placement_total
                        .with_label_values(&[&placement.server_type, &placement.location])
                        .inc();
                    self.pending_placements.insert(server.id, placement);
                    self.nodes.push(ManagedNode {
                        hetzner_server_id: server.id,
                        hetzner_server_name: server.name,
                        created_at: server.created,
                        state: NodeState::Provisioning,
                    });
                    return true;
                }
                Err(CreateError::Unavailable(e)) => {
                    warn!(
                        error = %e,
                        server_type = %placement.server_type,
                        location = %placement.location,
                        "placement unavailable, trying the next one"
                    );
                    self.mark_unavailable(placement, config, metrics, now);
                }
                Err(CreateError::Other(e)) => {
                    warn!(error = %e, "failed to create Hetzner server");
                    metrics.scale_up_errors_total.inc();
                    return true;
                }
            }
        }

        if tried {
            warn!("no placement has capacity, waiting for the cooldown");
        } else {
            debug!("every placement is cooling down, not creating a server");
        }
        false
    }

    fn mark_unavailable(
        &mut self,
        placement: Placement,
        config: &Config,
        metrics: &Metrics,
        now: DateTime<Utc>,
    ) {
        metrics
            .placement_unavailable_total
            .with_label_values(&[&placement.server_type, &placement.location])
            .inc();
        let cooldown = Duration::seconds(config.placement_cooldown_secs as i64);
        self.unavailable_until.insert(placement, now + cooldown);
    }

    /// Forget provisioning servers that Hetzner accepted but never placed. They vanish
    /// from the API without an error, so their placement is treated as unavailable.
    pub async fn drop_vanished_servers(
        &mut self,
        listed: &[HetznerServer],
        config: &Config,
        hetzner: &dyn HetznerClient,
        metrics: &Metrics,
        now: DateTime<Utc>,
    ) {
        self.pending_placements.retain(|id, _| {
            self.nodes
                .iter()
                .any(|n| n.hetzner_server_id == *id && n.state == NodeState::Provisioning)
        });

        let missing: Vec<i64> = self
            .nodes
            .iter()
            .filter(|n| {
                n.state == NodeState::Provisioning
                    && (now - n.created_at).num_seconds() >= VANISH_GRACE_SECS
                    && !listed.iter().any(|s| s.id == n.hetzner_server_id)
            })
            .map(|n| n.hetzner_server_id)
            .collect();

        for server_id in missing {
            match hetzner.server_exists(server_id).await {
                Ok(true) => {}
                Ok(false) => {
                    self.nodes.retain(|n| n.hetzner_server_id != server_id);
                    metrics.scale_up_errors_total.inc();
                    let placement = self.pending_placements.remove(&server_id);
                    warn!(
                        server_id,
                        server_type = placement.as_ref().map(|p| p.server_type.as_str()),
                        location = placement.as_ref().map(|p| p.location.as_str()),
                        "server vanished before it was placed"
                    );
                    if let Some(placement) = placement {
                        self.mark_unavailable(placement, config, metrics, now);
                    }
                }
                Err(e) => warn!(error = %e, server_id, "failed to look up missing server"),
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

    /// K8s nodes whose Hetzner server is gone (deleted by hand, or lost by Hetzner).
    /// Teardown only runs for servers that still exist, so nothing else cleans these up.
    pub fn find_orphaned_nodes(
        &mut self,
        servers: &[HetznerServer],
        k8s_nodes: &[K8sNode],
        now: DateTime<Utc>,
    ) -> Vec<String> {
        // A Ready node has a live server behind it, whatever the Hetzner listing says.
        let orphans: Vec<&K8sNode> = k8s_nodes
            .iter()
            .filter(|n| !n.ready && !servers.iter().any(|s| s.name == n.name))
            .collect();
        self.orphaned_since
            .retain(|name, _| orphans.iter().any(|n| n.name == *name));

        orphans
            .into_iter()
            .filter(|n| {
                let since = *self.orphaned_since.entry(n.name.clone()).or_insert(now);
                (now - since).num_seconds() >= ORPHAN_GRACE_SECS
            })
            .map(|n| n.name.clone())
            .collect()
    }

    /// Deregister the orphaned node's runner and delete the node from k8s.
    pub async fn remove_orphaned_node(
        &mut self,
        node_name: &str,
        k8s_pods: &[K8sPod],
        runners: &[Runner],
        gitea: &dyn GiteaClient,
        kube: &dyn KubeClient,
        metrics: &Metrics,
    ) {
        warn!(node = %node_name, "hetzner server is gone, removing orphaned k8s node");
        let runner = k8s_pods
            .iter()
            .filter(|p| p.node_name.as_deref() == Some(node_name))
            .find_map(|p| runners.iter().find(|r| r.name == p.name));
        if let Some(runner) = runner {
            match gitea.delete_runner(runner.id).await {
                Ok(()) => metrics.runners_deregistered_total.inc(),
                Err(e) => warn!(error = %e, runner = %runner.name, "failed to deregister runner"),
            }
        }
        match kube.delete_node(node_name).await {
            Ok(()) => {
                metrics.orphaned_nodes_removed_total.inc();
                self.orphaned_since.remove(node_name);
            }
            Err(e) => warn!(error = %e, node = %node_name, "failed to delete orphaned k8s node"),
        }
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

    fn fallback_config() -> Config {
        Config {
            poll_interval_secs: 5,
            max_nodes: 5,
            idle_timeout_mins: 5,
            billing_window_mins: 5,
            provisioning_timeout_secs: 600,
            hetzner_server_types: vec!["cx53".into(), "ccx33".into()],
            hetzner_locations: vec!["fsn1".into()],
            placement_cooldown_secs: 300,
            hetzner_image: "ubuntu-24.04".into(),
            hetzner_api_token: "test".into(),
            cluster_secret: "test-token".into(),
            gitea_api_url: "http://localhost:3000".into(),
            gitea_admin_token: "test".into(),
            pushgateway_url: "http://localhost:9091".into(),
            runner_namespace: "gitea-runners".into(),
            k3s_agent_args: String::new(),
        }
    }

    #[tokio::test]
    async fn preferred_placement_is_retried_after_cooldown() {
        let config = fallback_config();
        let metrics = Metrics::new();
        let hetzner = MockHetznerClient::new();
        let cx53 = Placement {
            server_type: "cx53".into(),
            location: "fsn1".into(),
        };
        let ccx33 = Placement {
            server_type: "ccx33".into(),
            location: "fsn1".into(),
        };
        hetzner.unavailable.lock().unwrap().push(cx53.clone());

        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        let now = Utc::now();

        mgr.scale_up(1, &config, &hetzner, &metrics, now).await;
        // Still cooling down: goes straight to the fallback.
        mgr.scale_up(1, &config, &hetzner, &metrics, now + Duration::seconds(299))
            .await;
        // Stock is back and the cooldown is over: the preferred type is used again.
        hetzner.unavailable.lock().unwrap().clear();
        mgr.scale_up(1, &config, &hetzner, &metrics, now + Duration::seconds(301))
            .await;

        assert_eq!(
            *hetzner.create_attempts.lock().unwrap(),
            [cx53.clone(), ccx33.clone(), ccx33, cx53]
        );
        assert_eq!(mgr.nodes.len(), 3);
    }

    #[tokio::test]
    async fn missing_server_is_kept_during_grace_and_while_it_exists() {
        let config = fallback_config();
        let metrics = Metrics::new();
        let hetzner = MockHetznerClient::new();
        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        let now = Utc::now();

        mgr.scale_up(1, &config, &hetzner, &metrics, now).await;

        // Not listed yet, but too young to be looked up.
        mgr.drop_vanished_servers(&[], &config, &hetzner, &metrics, now)
            .await;
        assert_eq!(mgr.nodes.len(), 1);

        // Not listed after the grace period, but Hetzner still knows it.
        let later = now + Duration::seconds(VANISH_GRACE_SECS + 1);
        mgr.drop_vanished_servers(&[], &config, &hetzner, &metrics, later)
            .await;
        assert_eq!(mgr.nodes.len(), 1);

        // Gone for good.
        hetzner.servers.lock().unwrap().clear();
        mgr.drop_vanished_servers(&[], &config, &hetzner, &metrics, later)
            .await;
        assert!(mgr.nodes.is_empty());
    }

    fn k8s_node(name: &str, ready: bool) -> K8sNode {
        K8sNode {
            name: name.into(),
            unschedulable: false,
            ready,
        }
    }

    fn hetzner_server(name: &str) -> HetznerServer {
        HetznerServer {
            id: 1,
            name: name.into(),
            created: Utc::now(),
            labels: HashMap::new(),
        }
    }

    #[test]
    fn orphaned_node_is_reported_after_grace() {
        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        let nodes = [k8s_node("ci-runner-gone", false)];
        let now = Utc::now();

        assert!(mgr.find_orphaned_nodes(&[], &nodes, now).is_empty());
        let later = now + Duration::seconds(ORPHAN_GRACE_SECS);
        assert_eq!(
            mgr.find_orphaned_nodes(&[], &nodes, later),
            ["ci-runner-gone"]
        );
    }

    #[test]
    fn node_with_server_or_heartbeat_is_never_orphaned() {
        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        let nodes = [
            // Booting or rebooting: NotReady, but the server exists.
            k8s_node("ci-runner-booting", false),
            // Missing from the Hetzner listing, but the kubelet is alive.
            k8s_node("ci-runner-unlisted", true),
        ];
        let servers = [hetzner_server("ci-runner-booting")];
        let now = Utc::now();

        mgr.find_orphaned_nodes(&servers, &nodes, now);
        let later = now + Duration::seconds(ORPHAN_GRACE_SECS * 10);
        assert!(mgr.find_orphaned_nodes(&servers, &nodes, later).is_empty());
    }

    #[test]
    fn orphan_grace_restarts_when_node_recovers() {
        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        let now = Utc::now();
        let step = Duration::seconds(ORPHAN_GRACE_SECS - 1);

        mgr.find_orphaned_nodes(&[], &[k8s_node("ci-runner-flaky", false)], now);
        mgr.find_orphaned_nodes(&[], &[k8s_node("ci-runner-flaky", true)], now + step);
        let orphans =
            mgr.find_orphaned_nodes(&[], &[k8s_node("ci-runner-flaky", false)], now + step * 2);
        assert!(orphans.is_empty());
    }

    #[tokio::test]
    async fn orphaned_node_removal_deregisters_runner_and_deletes_node() {
        let metrics = Metrics::new();
        let gitea = MockGiteaClient::new();
        let kube = MockKubeClient::new();
        let mut mgr = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());

        let pods = [
            K8sPod {
                name: "runner-gone".into(),
                namespace: "gitea-runners".into(),
                node_name: Some("ci-runner-gone".into()),
            },
            K8sPod {
                name: "runner-alive".into(),
                namespace: "gitea-runners".into(),
                node_name: Some("ci-runner-alive".into()),
            },
        ];
        let runners = [
            make_runner(7, "runner-gone", true, false),
            make_runner(8, "runner-alive", true, false),
        ];

        mgr.remove_orphaned_node("ci-runner-gone", &pods, &runners, &gitea, &kube, &metrics)
            .await;

        assert_eq!(*gitea.delete_runner_calls.lock().unwrap(), [7]);
        assert_eq!(*kube.delete_node_calls.lock().unwrap(), ["ci-runner-gone"]);
    }

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
