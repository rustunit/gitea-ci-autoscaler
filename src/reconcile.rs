use chrono::{DateTime, Utc};
use tracing::{info, warn};

use crate::gitea::{Job, Runner};
use crate::hetzner::HetznerServer;
use crate::k8s::{K8sNode, K8sPod};
use crate::node_manager::{ManagedNode, NodeState};

/// Reconcile in-memory state from external APIs.
pub async fn reconcile(
    hetzner_servers: &[HetznerServer],
    k8s_nodes: &[K8sNode],
    k8s_pods: &[K8sPod],
    runners: &[Runner],
    in_progress_jobs: &[Job],
    now: DateTime<Utc>,
    provisioning_timeout: std::time::Duration,
) -> (Vec<ManagedNode>, Vec<ReconcileAction>) {
    let mut nodes = Vec::new();
    let mut actions = Vec::new();

    for server in hetzner_servers {
        let k8s_node = k8s_nodes.iter().find(|n| n.name == server.name);

        match k8s_node {
            Some(k8s_node) => {
                if k8s_node.unschedulable {
                    // Cordoned node with no runner → mid-teardown (Draining)
                    let has_runner = k8s_pods
                        .iter()
                        .find(|p| p.node_name.as_deref() == Some(&k8s_node.name))
                        .and_then(|pod| runners.iter().find(|r| r.name == pod.name))
                        .is_some();

                    if !has_runner {
                        nodes.push(ManagedNode {
                            hetzner_server_id: server.id,
                            hetzner_server_name: server.name.clone(),
                            created_at: server.created,
                            state: NodeState::Draining {
                                k8s_node_name: k8s_node.name.clone(),
                            },
                        });
                        continue;
                    }
                }

                // Node exists in k8s — find the runner pod on it
                let pod = k8s_pods
                    .iter()
                    .find(|p| p.node_name.as_deref() == Some(&k8s_node.name));

                let runner = pod.and_then(|p| runners.iter().find(|r| r.name == p.name));

                match runner {
                    Some(runner) => {
                        let is_busy = runner.busy
                            || in_progress_jobs
                                .iter()
                                .any(|j| j.runner_id == Some(runner.id));

                        if is_busy {
                            nodes.push(ManagedNode {
                                hetzner_server_id: server.id,
                                hetzner_server_name: server.name.clone(),
                                created_at: server.created,
                                state: NodeState::Busy {
                                    k8s_node_name: k8s_node.name.clone(),
                                    gitea_runner_id: runner.id,
                                    gitea_runner_name: runner.name.clone(),
                                },
                            });
                        } else {
                            nodes.push(ManagedNode {
                                hetzner_server_id: server.id,
                                hetzner_server_name: server.name.clone(),
                                created_at: server.created,
                                state: NodeState::Idle {
                                    k8s_node_name: k8s_node.name.clone(),
                                    gitea_runner_id: runner.id,
                                    gitea_runner_name: runner.name.clone(),
                                    idle_since: now,
                                },
                            });
                        }
                    }
                    None => {
                        // k8s node exists but no runner pod/registration yet
                        nodes.push(ManagedNode {
                            hetzner_server_id: server.id,
                            hetzner_server_name: server.name.clone(),
                            created_at: server.created,
                            state: NodeState::Provisioning,
                        });
                    }
                }
            }
            None => {
                // No k8s node — either still provisioning or stuck
                let age = now - server.created;
                let timeout =
                    chrono::Duration::from_std(provisioning_timeout).expect("valid duration");

                if age < timeout {
                    nodes.push(ManagedNode {
                        hetzner_server_id: server.id,
                        hetzner_server_name: server.name.clone(),
                        created_at: server.created,
                        state: NodeState::Provisioning,
                    });
                } else {
                    warn!(
                        server = %server.name,
                        age_secs = age.num_seconds(),
                        "reconcile: stuck server, scheduling deletion"
                    );
                    actions.push(ReconcileAction::DeleteStuckServer {
                        server_id: server.id,
                    });
                }
            }
        }
    }

    (nodes, actions)
}

#[derive(Debug, PartialEq)]
pub enum ReconcileAction {
    DeleteStuckServer { server_id: i64 },
}

/// Carry forward temporal state from the previous iteration into freshly reconciled nodes.
/// - Preserves teardown states (Deregistering/Draining/Removing) that reconcile can't reconstruct
/// - Preserves `idle_since` for nodes that were already Idle
/// - Detects Provisioning → Busy/Idle transitions and returns their durations
pub fn carry_forward_state(
    nodes: &mut Vec<ManagedNode>,
    previous: &[ManagedNode],
    now: DateTime<Utc>,
    provisioning_timeout: std::time::Duration,
) -> (Vec<f64>, Vec<ReconcileAction>) {
    let mut provisioning_durations = Vec::new();

    for node in nodes.iter_mut() {
        let prev = previous
            .iter()
            .find(|p| p.hetzner_server_id == node.hetzner_server_id);

        let Some(prev) = prev else {
            info!(
                server = %node.hetzner_server_name,
                state = node.state.state_name(),
                "new node discovered"
            );
            continue;
        };

        match (&mut node.state, &prev.state) {
            // Preserve idle_since when node was already idle
            (
                NodeState::Idle { idle_since, .. },
                NodeState::Idle {
                    idle_since: prev_idle_since,
                    ..
                },
            ) => {
                *idle_since = *prev_idle_since;
            }
            // Busy → Idle
            (
                NodeState::Idle {
                    gitea_runner_id,
                    gitea_runner_name,
                    ..
                },
                NodeState::Busy { .. },
            ) => {
                info!(
                    server = %node.hetzner_server_name,
                    runner_id = *gitea_runner_id,
                    runner = %gitea_runner_name,
                    "busy -> idle"
                );
            }
            // Idle → Busy
            (
                NodeState::Busy {
                    gitea_runner_id,
                    gitea_runner_name,
                    ..
                },
                NodeState::Idle { .. },
            ) => {
                info!(
                    server = %node.hetzner_server_name,
                    runner_id = *gitea_runner_id,
                    runner = %gitea_runner_name,
                    "idle -> busy"
                );
            }
            // Provisioning → Busy: record provisioning duration
            (
                NodeState::Busy {
                    gitea_runner_id,
                    gitea_runner_name,
                    ..
                },
                NodeState::Provisioning,
            ) => {
                let duration_secs = (now - node.created_at).num_seconds() as f64;
                provisioning_durations.push(duration_secs);
                info!(
                    server = %node.hetzner_server_name,
                    runner_id = *gitea_runner_id,
                    runner = %gitea_runner_name,
                    state = "busy",
                    duration_secs,
                    "provisioning complete"
                );
            }
            // Provisioning → Idle: record provisioning duration
            (
                NodeState::Idle {
                    gitea_runner_id,
                    gitea_runner_name,
                    ..
                },
                NodeState::Provisioning,
            ) => {
                let duration_secs = (now - node.created_at).num_seconds() as f64;
                provisioning_durations.push(duration_secs);
                info!(
                    server = %node.hetzner_server_name,
                    runner_id = *gitea_runner_id,
                    runner = %gitea_runner_name,
                    state = "idle",
                    duration_secs,
                    "provisioning complete"
                );
            }
            // Preserve teardown states — reconcile can't reconstruct these
            (
                _,
                NodeState::Deregistering { .. } | NodeState::Draining { .. } | NodeState::Removing,
            ) => {
                node.state = prev.state.clone();
            }
            _ => {}
        }
    }

    // Re-add nodes from previous state that reconcile couldn't see:
    // - Removing: server may already be deleted from Hetzner but we need to confirm
    // - Provisioning: server may not yet appear in Hetzner list_servers due to API delay
    let mut actions = Vec::new();
    let timeout = chrono::Duration::from_std(provisioning_timeout).expect("valid duration");
    for prev in previous {
        let still_tracked = nodes
            .iter()
            .any(|n| n.hetzner_server_id == prev.hetzner_server_id);
        if still_tracked {
            continue;
        }
        match prev.state {
            NodeState::Removing => {
                nodes.push(prev.clone());
            }
            NodeState::Provisioning => {
                let age = now - prev.created_at;
                if age < timeout {
                    nodes.push(prev.clone());
                } else {
                    warn!(
                        server = %prev.hetzner_server_name,
                        server_id = prev.hetzner_server_id,
                        age_secs = age.num_seconds(),
                        "ghost provisioning node timed out, scheduling deletion"
                    );
                    actions.push(ReconcileAction::DeleteStuckServer {
                        server_id: prev.hetzner_server_id,
                    });
                }
            }
            _ => {}
        }
    }

    (provisioning_durations, actions)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::gitea::{Job, Runner, RunnerLabel};
    use crate::hetzner::HetznerServer;
    use crate::k8s::{K8sNode, K8sPod};
    use chrono::Duration;

    fn make_server(id: i64, name: &str, age_mins: i64) -> HetznerServer {
        HetznerServer {
            id,
            name: name.to_string(),
            created: Utc::now() - Duration::minutes(age_mins),
            labels: {
                let mut m = std::collections::HashMap::new();
                m.insert("managed-by".to_string(), "gitea-ci-autoscaler".to_string());
                m
            },
        }
    }

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

    fn make_job(id: u64, runner_id: u64, runner_name: &str) -> Job {
        Job {
            id,
            name: format!("job-{id}"),
            labels: vec!["self-hosted".into(), "linux".into()],
            status: "in_progress".into(),
            runner_id: Some(runner_id),
            runner_name: Some(runner_name.to_string()),
        }
    }

    #[tokio::test]
    async fn reconcile_clean_state() {
        let servers = vec![
            make_server(1, "ci-runner-1", 10),
            make_server(2, "ci-runner-2", 5),
        ];
        let k8s_nodes = vec![
            K8sNode {
                name: "ci-runner-1".into(),
                unschedulable: false,
            },
            K8sNode {
                name: "ci-runner-2".into(),
                unschedulable: false,
            },
        ];
        let pods = vec![
            K8sPod {
                name: "runner-1".into(),
                namespace: "gitea-runners".into(),
                node_name: Some("ci-runner-1".into()),
            },
            K8sPod {
                name: "runner-2".into(),
                namespace: "gitea-runners".into(),
                node_name: Some("ci-runner-2".into()),
            },
        ];
        let runners = vec![
            make_runner(100, "runner-1", true, true),
            make_runner(101, "runner-2", true, false),
        ];
        let jobs = vec![make_job(1, 100, "runner-1")];

        let (nodes, actions) = reconcile(
            &servers,
            &k8s_nodes,
            &pods,
            &runners,
            &jobs,
            Utc::now(),
            std::time::Duration::from_secs(600),
        )
        .await;

        assert_eq!(nodes.len(), 2);
        assert!(actions.is_empty());
        assert!(matches!(nodes[0].state, NodeState::Busy { .. }));
        assert!(matches!(nodes[1].state, NodeState::Idle { .. }));
    }

    #[tokio::test]
    async fn reconcile_provisioning_server() {
        let servers = vec![make_server(1, "ci-runner-1", 2)]; // 2 min old
        let (nodes, actions) = reconcile(
            &servers,
            &[],
            &[],
            &[],
            &[],
            Utc::now(),
            std::time::Duration::from_secs(600),
        )
        .await;

        assert_eq!(nodes.len(), 1);
        assert!(actions.is_empty());
        assert!(matches!(nodes[0].state, NodeState::Provisioning));
    }

    #[tokio::test]
    async fn reconcile_stuck_server() {
        let servers = vec![make_server(1, "ci-runner-1", 15)]; // 15 min old
        let (nodes, actions) = reconcile(
            &servers,
            &[],
            &[],
            &[],
            &[],
            Utc::now(),
            std::time::Duration::from_secs(600),
        )
        .await;

        assert!(nodes.is_empty());
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ReconcileAction::DeleteStuckServer { server_id: 1 }
        );
    }

    #[tokio::test]
    async fn reconcile_mid_teardown() {
        let servers = vec![make_server(1, "ci-runner-1", 30)];
        let k8s_nodes = vec![K8sNode {
            name: "ci-runner-1".into(),
            unschedulable: true,
        }];
        // No runner pod
        let (nodes, actions) = reconcile(
            &servers,
            &k8s_nodes,
            &[],
            &[],
            &[],
            Utc::now(),
            std::time::Duration::from_secs(600),
        )
        .await;

        assert_eq!(nodes.len(), 1);
        assert!(actions.is_empty());
        assert!(matches!(nodes[0].state, NodeState::Draining { .. }));
    }

    #[tokio::test]
    async fn reconcile_no_servers() {
        let (nodes, actions) = reconcile(
            &[],
            &[],
            &[],
            &[],
            &[],
            Utc::now(),
            std::time::Duration::from_secs(600),
        )
        .await;

        assert!(nodes.is_empty());
        assert!(actions.is_empty());
    }

    #[test]
    fn carry_forward_preserves_idle_since() {
        let now = Utc::now();
        let original_idle_since = now - Duration::minutes(10);

        let previous = vec![ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: now - Duration::minutes(30),
            state: NodeState::Idle {
                k8s_node_name: "ci-runner-1".into(),
                gitea_runner_id: 100,
                gitea_runner_name: "runner-1".into(),
                idle_since: original_idle_since,
            },
        }];

        let mut nodes = vec![ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: now - Duration::minutes(30),
            state: NodeState::Idle {
                k8s_node_name: "ci-runner-1".into(),
                gitea_runner_id: 100,
                gitea_runner_name: "runner-1".into(),
                idle_since: now, // reconcile sets this to now
            },
        }];

        let timeout = std::time::Duration::from_secs(600);
        let (durations, actions) = carry_forward_state(&mut nodes, &previous, now, timeout);
        assert!(durations.is_empty());
        assert!(actions.is_empty());
        if let NodeState::Idle { idle_since, .. } = &nodes[0].state {
            assert_eq!(*idle_since, original_idle_since);
        } else {
            panic!("expected Idle state");
        }
    }

    #[test]
    fn carry_forward_records_provisioning_duration() {
        let now = Utc::now();

        let previous = vec![ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: now - Duration::seconds(65),
            state: NodeState::Provisioning,
        }];

        let mut nodes = vec![ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: now - Duration::seconds(65),
            state: NodeState::Busy {
                k8s_node_name: "ci-runner-1".into(),
                gitea_runner_id: 100,
                gitea_runner_name: "runner-1".into(),
            },
        }];

        let timeout = std::time::Duration::from_secs(600);
        let (durations, actions) = carry_forward_state(&mut nodes, &previous, now, timeout);
        assert_eq!(durations.len(), 1);
        assert!((durations[0] - 65.0).abs() < 1.0);
        assert!(actions.is_empty());
    }

    #[test]
    fn carry_forward_preserves_deregistering() {
        let now = Utc::now();

        // Previous: node was in Deregistering (runner already deleted)
        let previous = vec![ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: now - Duration::minutes(30),
            state: NodeState::Deregistering {
                k8s_node_name: "ci-runner-1".into(),
                gitea_runner_id: 100,
            },
        }];

        // Reconcile misclassifies it (k8s node exists, no runner -> Provisioning)
        let mut nodes = vec![ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: now - Duration::minutes(30),
            state: NodeState::Provisioning,
        }];

        let timeout = std::time::Duration::from_secs(600);
        let (durations, actions) = carry_forward_state(&mut nodes, &previous, now, timeout);
        assert!(durations.is_empty());
        assert!(actions.is_empty());
        assert!(matches!(nodes[0].state, NodeState::Deregistering { .. }));
    }

    #[test]
    fn carry_forward_preserves_removing_without_server() {
        let now = Utc::now();

        // Previous: node was in Removing (k8s node deleted, Hetzner delete pending)
        let previous = vec![ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: now - Duration::minutes(30),
            state: NodeState::Removing,
        }];

        // Reconcile doesn't see it at all (Hetzner server already deleted)
        let mut nodes: Vec<ManagedNode> = vec![];

        let timeout = std::time::Duration::from_secs(600);
        let (durations, actions) = carry_forward_state(&mut nodes, &previous, now, timeout);
        assert!(durations.is_empty());
        assert!(actions.is_empty());
        // Should be re-added so teardown_step can retry the Hetzner deletion
        assert_eq!(nodes.len(), 1);
        assert!(matches!(nodes[0].state, NodeState::Removing));
    }

    #[test]
    fn carry_forward_no_previous() {
        let now = Utc::now();

        let mut nodes = vec![ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: now - Duration::minutes(5),
            state: NodeState::Provisioning,
        }];

        let timeout = std::time::Duration::from_secs(600);
        let (durations, actions) = carry_forward_state(&mut nodes, &[], now, timeout);
        assert!(durations.is_empty());
        assert!(actions.is_empty());
    }

    #[test]
    fn carry_forward_ghost_provisioning_times_out() {
        let now = Utc::now();
        let timeout = std::time::Duration::from_secs(600);

        // Previous: node was Provisioning, created 15 mins ago (past 10 min timeout)
        let previous = vec![ManagedNode {
            hetzner_server_id: 42,
            hetzner_server_name: "ci-runner-ghost".into(),
            created_at: now - Duration::minutes(15),
            state: NodeState::Provisioning,
        }];

        // Reconcile doesn't see it (vanished from Hetzner list_servers)
        let mut nodes: Vec<ManagedNode> = vec![];

        let (durations, actions) = carry_forward_state(&mut nodes, &previous, now, timeout);
        assert!(durations.is_empty());
        // Should NOT be re-added
        assert!(nodes.is_empty());
        // Should emit a delete action
        assert_eq!(actions.len(), 1);
        assert_eq!(
            actions[0],
            ReconcileAction::DeleteStuckServer { server_id: 42 }
        );
    }

    #[test]
    fn carry_forward_ghost_provisioning_within_timeout() {
        let now = Utc::now();
        let timeout = std::time::Duration::from_secs(600);

        // Previous: node was Provisioning, created 2 mins ago (within timeout)
        let previous = vec![ManagedNode {
            hetzner_server_id: 42,
            hetzner_server_name: "ci-runner-new".into(),
            created_at: now - Duration::minutes(2),
            state: NodeState::Provisioning,
        }];

        // Reconcile doesn't see it yet (API delay)
        let mut nodes: Vec<ManagedNode> = vec![];

        let (durations, actions) = carry_forward_state(&mut nodes, &previous, now, timeout);
        assert!(durations.is_empty());
        assert!(actions.is_empty());
        // Should be re-added (within grace window)
        assert_eq!(nodes.len(), 1);
        assert!(matches!(nodes[0].state, NodeState::Provisioning));
    }
}
