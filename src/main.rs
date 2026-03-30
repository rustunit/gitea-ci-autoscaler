mod cloud_init;
mod config;
mod gitea;
mod hetzner;
mod k8s;
mod metrics;
mod node_manager;
mod reconcile;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod mocks;

use chrono::Utc;
use std::time::Duration;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::gitea::{GiteaClient, JobStatus, RealGiteaClient, filter_linux_jobs};
use crate::hetzner::{HetznerClient, RealHetznerClient};
use crate::k8s::{KubeClient, RealKubeClient};
use crate::metrics::Metrics;
use crate::node_manager::{NodeManager, permanent_runner_capacity};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let config = Config::from_env()?;
    info!("starting gitea-ci-autoscaler");

    let gitea = RealGiteaClient::new(
        config.gitea_api_url.clone(),
        config.gitea_admin_token.clone(),
    );
    let hetzner = RealHetznerClient::new(
        config.hetzner_api_token.clone(),
        config.hetzner_server_type.clone(),
        config.hetzner_location.clone(),
        config.hetzner_image.clone(),
    )
    .await?;
    let kube = RealKubeClient::new().await?;
    let metrics = Metrics::new();

    let k3s_version = kube.get_k3s_version().await?;
    let master_ip = kube.get_master_ip().await?;
    info!(k3s_version = %k3s_version, master_ip = %master_ip, "detected cluster info");

    let mut manager = NodeManager::new(k3s_version, master_ip);

    // Main control loop
    let poll_interval = Duration::from_secs(config.poll_interval_secs);
    let mut consecutive_failures: u64 = 0;
    loop {
        let loop_start = std::time::Instant::now();

        match run_loop_iteration(&mut manager, &gitea, &hetzner, &kube, &metrics, &config).await {
            Ok(()) => {
                if consecutive_failures > 0 {
                    info!(
                        previous_failures = consecutive_failures,
                        "control loop recovered"
                    );
                    consecutive_failures = 0;
                }
            }
            Err(e) => {
                consecutive_failures += 1;
                error!(error = %e, consecutive_failures, "control loop iteration failed");
            }
        }

        let elapsed = loop_start.elapsed();
        metrics.loop_duration.observe(elapsed.as_secs_f64());

        if let Err(e) = metrics.push(&config.pushgateway_url).await {
            warn!(error = %e, "failed to push metrics");
        }

        if poll_interval > elapsed {
            tokio::time::sleep(poll_interval - elapsed).await;
        }
    }
}

async fn run_loop_iteration(
    manager: &mut NodeManager,
    gitea: &dyn GiteaClient,
    hetzner: &dyn HetznerClient,
    kube: &dyn KubeClient,
    metrics: &Metrics,
    config: &Config,
) -> anyhow::Result<()> {
    // 1. Query Gitea
    let gitea_start = std::time::Instant::now();
    let waiting_jobs = match gitea.list_jobs(JobStatus::Waiting).await {
        Ok(j) => j,
        Err(e) => {
            warn!(error = %e, "Gitea API unavailable, skipping iteration");
            return Ok(());
        }
    };
    let queued_jobs = match gitea.list_jobs(JobStatus::Queued).await {
        Ok(j) => j,
        Err(e) => {
            warn!(error = %e, "Gitea API unavailable, skipping iteration");
            return Ok(());
        }
    };
    let in_progress_jobs = match gitea.list_jobs(JobStatus::InProgress).await {
        Ok(j) => j,
        Err(e) => {
            warn!(error = %e, "Gitea API unavailable, skipping iteration");
            return Ok(());
        }
    };
    let runners = match gitea.list_runners().await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "Gitea API unavailable, skipping iteration");
            return Ok(());
        }
    };
    metrics
        .gitea_api_duration
        .observe(gitea_start.elapsed().as_secs_f64());

    let waiting_linux =
        filter_linux_jobs(&waiting_jobs).len() + filter_linux_jobs(&queued_jobs).len();
    let running_linux = filter_linux_jobs(&in_progress_jobs).len();

    metrics.linux_jobs_waiting.set(waiting_linux as f64);
    metrics.linux_jobs_running.set(running_linux as f64);

    // 2. Query Hetzner
    let hetzner_start = std::time::Instant::now();
    let hetzner_servers = hetzner.list_servers().await?;
    metrics
        .hetzner_api_duration
        .observe(hetzner_start.elapsed().as_secs_f64());

    // 3. Query Kubernetes
    let k8s_start = std::time::Instant::now();
    let k8s_nodes = kube.list_nodes("managed-by=gitea-ci-autoscaler").await?;
    let k8s_pods = kube
        .list_pods(&config.runner_namespace, "app=gitea-actions-runner")
        .await?;
    metrics
        .k8s_api_duration
        .observe(k8s_start.elapsed().as_secs_f64());

    // 4. Reconcile state from external APIs
    let linux_in_progress: Vec<_> = in_progress_jobs
        .into_iter()
        .filter(|j| j.is_linux())
        .collect();

    let provisioning_timeout = Duration::from_secs(config.provisioning_timeout_secs);
    let now = Utc::now();

    let (mut nodes, actions) = reconcile::reconcile(
        &hetzner_servers,
        &k8s_nodes,
        &k8s_pods,
        &runners,
        &linux_in_progress,
        now,
        provisioning_timeout,
    )
    .await;

    // Carry forward temporal state from previous iteration
    let (provisioning_durations, carry_forward_actions) =
        reconcile::carry_forward_state(&mut nodes, &manager.nodes, now, provisioning_timeout);
    for duration in provisioning_durations {
        metrics.provisioning_duration.observe(duration);
    }

    manager.nodes = nodes;

    // Handle stuck servers (from both reconcile and carry_forward), deduplicated
    let mut all_actions: Vec<_> = actions.into_iter().chain(carry_forward_actions).collect();
    all_actions.dedup_by_key(|a| match a {
        reconcile::ReconcileAction::DeleteStuckServer { server_id } => *server_id,
    });
    for action in all_actions {
        match action {
            reconcile::ReconcileAction::DeleteStuckServer { server_id } => {
                manager
                    .delete_stuck_server(server_id, hetzner, metrics)
                    .await;
            }
        }
    }

    // 5. Compute permanent runner capacity
    let managed_runner_names: Vec<String> = manager
        .nodes
        .iter()
        .filter_map(|n| match &n.state {
            node_manager::NodeState::Busy {
                gitea_runner_name, ..
            }
            | node_manager::NodeState::Idle {
                gitea_runner_name, ..
            } => Some(gitea_runner_name.clone()),
            _ => None,
        })
        .collect();

    let perm_capacity = permanent_runner_capacity(&runners, &managed_runner_names);

    // 6. Scale up
    let scale_up_count = manager.compute_scale_up(waiting_linux, perm_capacity, config.max_nodes);

    metrics
        .desired_nodes
        .set((manager.nodes.len() + scale_up_count) as f64);

    if scale_up_count > 0 {
        info!(count = scale_up_count, "scaling up");
        manager
            .scale_up(scale_up_count, config, hetzner, metrics)
            .await;
    }

    // 7. Scale down
    let idle_timeout = Duration::from_secs(config.idle_timeout_mins * 60);
    let candidates =
        manager.find_teardown_candidates(now, idle_timeout, config.billing_window_mins);

    // Process in reverse order so indices stay valid
    for &index in candidates.iter().rev() {
        manager
            .teardown_step(index, gitea, kube, hetzner, metrics)
            .await;
    }

    // Also advance any nodes already in teardown states
    let teardown_indices: Vec<usize> = manager
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| {
            matches!(
                n.state,
                node_manager::NodeState::Deregistering { .. }
                    | node_manager::NodeState::Draining { .. }
                    | node_manager::NodeState::Removing
            )
            .then_some(i)
        })
        .collect();

    for &index in teardown_indices.iter().rev() {
        manager
            .teardown_step(index, gitea, kube, hetzner, metrics)
            .await;
    }

    // 8. Update metrics
    manager.update_metrics(metrics, now);

    info!(
        managed_nodes = manager.nodes.len(),
        waiting_linux, running_linux, perm_capacity, scale_up_count, "loop iteration complete"
    );

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::gitea::{Job, Runner, RunnerLabel};
    use crate::mocks::*;
    use chrono::Duration as ChronoDuration;

    fn make_config() -> Config {
        Config {
            poll_interval_secs: 5,
            max_nodes: 5,
            idle_timeout_mins: 5,
            billing_window_mins: 5,
            provisioning_timeout_secs: 600,
            hetzner_server_type: "ccx33".into(),
            hetzner_location: "nbg1".into(),
            hetzner_image: "ubuntu-24.04".into(),
            hetzner_api_token: "test".into(),
            cluster_secret: "test-token".into(),
            gitea_api_url: "http://localhost:3000".into(),
            gitea_admin_token: "test".into(),
            pushgateway_url: "http://localhost:9091".into(),
            runner_namespace: "gitea-runners".into(),
        }
    }

    fn make_waiting_linux_job(id: u64) -> Job {
        Job {
            id,
            name: format!("job-{id}"),
            labels: vec!["self-hosted".into(), "linux".into()],
            status: "waiting".into(),
            runner_id: None,
            runner_name: None,
        }
    }

    fn make_in_progress_job(id: u64, runner_id: u64, runner_name: &str) -> Job {
        Job {
            id,
            name: format!("job-{id}"),
            labels: vec!["self-hosted".into(), "linux".into()],
            status: "in_progress".into(),
            runner_id: Some(runner_id),
            runner_name: Some(runner_name.to_string()),
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

    // --- Full loop tests ---

    #[tokio::test]
    async fn full_loop_scale_up() {
        let config = make_config();
        let metrics = Metrics::new();
        let mock_gitea = MockGiteaClient::new();
        let mock_hetzner = MockHetznerClient::new();
        let mock_kube = MockKubeClient::new();

        // 2 waiting linux jobs, no nodes
        mock_gitea
            .jobs
            .lock()
            .unwrap()
            .extend(vec![make_waiting_linux_job(1), make_waiting_linux_job(2)]);

        let mut manager = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());

        run_loop_iteration(
            &mut manager,
            &mock_gitea,
            &mock_hetzner,
            &mock_kube,
            &metrics,
            &config,
        )
        .await
        .unwrap();

        let create_calls = mock_hetzner.create_calls.lock().unwrap();
        assert_eq!(create_calls.len(), 2);
        assert_eq!(manager.nodes.len(), 2);
    }

    #[tokio::test]
    async fn full_loop_no_action() {
        let config = make_config();
        let metrics = Metrics::new();
        let mock_gitea = MockGiteaClient::new();
        let mock_hetzner = MockHetznerClient::new();
        let mock_kube = MockKubeClient::new();

        let created = Utc::now() - ChronoDuration::minutes(10);

        // Set up external state: Hetzner server + k8s node + pod + busy runner + in-progress job
        mock_hetzner
            .servers
            .lock()
            .unwrap()
            .push(crate::hetzner::HetznerServer {
                id: 1,
                name: "ci-runner-1".into(),
                created,
                labels: std::collections::HashMap::new(),
            });
        mock_kube.nodes.lock().unwrap().push(crate::k8s::K8sNode {
            name: "ci-runner-1".into(),
            unschedulable: false,
        });
        mock_kube.pods.lock().unwrap().push(crate::k8s::K8sPod {
            name: "runner-1".into(),
            namespace: "gitea-runners".into(),
            node_name: Some("ci-runner-1".into()),
        });
        mock_gitea
            .runners
            .lock()
            .unwrap()
            .push(make_runner(100, "runner-1", true, true));
        mock_gitea
            .jobs
            .lock()
            .unwrap()
            .push(make_in_progress_job(1, 100, "runner-1"));

        let mut manager = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());

        run_loop_iteration(
            &mut manager,
            &mock_gitea,
            &mock_hetzner,
            &mock_kube,
            &metrics,
            &config,
        )
        .await
        .unwrap();

        let create_calls = mock_hetzner.create_calls.lock().unwrap();
        assert_eq!(create_calls.len(), 0);
        assert_eq!(manager.nodes.len(), 1);
        assert!(matches!(
            manager.nodes[0].state,
            node_manager::NodeState::Busy { .. }
        ));
    }

    #[tokio::test]
    async fn full_loop_scale_down() {
        let config = make_config();
        let metrics = Metrics::new();
        let mock_gitea = MockGiteaClient::new();
        let mock_hetzner = MockHetznerClient::new();
        let mock_kube = MockKubeClient::new();

        let now = Utc::now();
        let created = now - ChronoDuration::minutes(56);

        // Set up external state: Hetzner server + k8s node + pod + idle runner
        mock_hetzner
            .servers
            .lock()
            .unwrap()
            .push(crate::hetzner::HetznerServer {
                id: 1,
                name: "ci-runner-1".into(),
                created,
                labels: std::collections::HashMap::new(),
            });
        mock_kube.nodes.lock().unwrap().push(crate::k8s::K8sNode {
            name: "ci-runner-1".into(),
            unschedulable: false,
        });
        mock_kube.pods.lock().unwrap().push(crate::k8s::K8sPod {
            name: "runner-1".into(),
            namespace: "gitea-runners".into(),
            node_name: Some("ci-runner-1".into()),
        });
        mock_gitea
            .runners
            .lock()
            .unwrap()
            .push(make_runner(100, "runner-1", true, false));

        // Pre-populate previous state with idle_since in the past so carry_forward preserves it
        let mut manager = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        manager.nodes.push(node_manager::ManagedNode {
            hetzner_server_id: 1,
            hetzner_server_name: "ci-runner-1".into(),
            created_at: created,
            state: node_manager::NodeState::Idle {
                k8s_node_name: "ci-runner-1".into(),
                gitea_runner_id: 100,
                gitea_runner_name: "runner-1".into(),
                idle_since: now - ChronoDuration::minutes(6), // past timeout
            },
        });

        run_loop_iteration(
            &mut manager,
            &mock_gitea,
            &mock_hetzner,
            &mock_kube,
            &metrics,
            &config,
        )
        .await
        .unwrap();

        // Should have started teardown (deregistered runner)
        let gitea_calls = mock_gitea.delete_runner_calls.lock().unwrap();
        assert_eq!(gitea_calls.len(), 1);
    }

    #[tokio::test]
    async fn full_loop_skip_on_gitea_failure() {
        let config = make_config();
        let metrics = Metrics::new();
        let mock_gitea = MockGiteaClient::new();
        let mock_hetzner = MockHetznerClient::new();
        let mock_kube = MockKubeClient::new();

        *mock_gitea.fail_list_jobs.lock().unwrap() = true;

        let mut manager = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());

        run_loop_iteration(
            &mut manager,
            &mock_gitea,
            &mock_hetzner,
            &mock_kube,
            &metrics,
            &config,
        )
        .await
        .unwrap();

        // No actions taken
        let create_calls = mock_hetzner.create_calls.lock().unwrap();
        assert_eq!(create_calls.len(), 0);
    }

    #[tokio::test]
    async fn no_double_scaleup_with_provisioning() {
        let config = make_config();
        let metrics = Metrics::new();
        let mock_gitea = MockGiteaClient::new();
        let mock_hetzner = MockHetznerClient::new();
        let mock_kube = MockKubeClient::new();

        // 1 waiting job
        mock_gitea
            .jobs
            .lock()
            .unwrap()
            .push(make_waiting_linux_job(1));

        // 1 Hetzner server with no k8s node yet → reconcile classifies as Provisioning
        mock_hetzner
            .servers
            .lock()
            .unwrap()
            .push(crate::hetzner::HetznerServer {
                id: 1,
                name: "ci-runner-1".into(),
                created: Utc::now() - ChronoDuration::seconds(30),
                labels: std::collections::HashMap::new(),
            });

        let mut manager = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());

        run_loop_iteration(
            &mut manager,
            &mock_gitea,
            &mock_hetzner,
            &mock_kube,
            &metrics,
            &config,
        )
        .await
        .unwrap();

        // Should NOT create another server
        let create_calls = mock_hetzner.create_calls.lock().unwrap();
        assert_eq!(create_calls.len(), 0);
        assert_eq!(manager.nodes.len(), 1);
        assert!(matches!(
            manager.nodes[0].state,
            node_manager::NodeState::Provisioning
        ));
    }

    #[tokio::test]
    async fn no_duplicate_delete_for_stuck_server() {
        let config = make_config();
        let metrics = Metrics::new();
        let mock_gitea = MockGiteaClient::new();
        let mock_hetzner = MockHetznerClient::new();
        let mock_kube = MockKubeClient::new();

        let created = Utc::now() - ChronoDuration::minutes(15); // past 10 min timeout

        // Hetzner still shows the stuck server (no k8s node) → reconcile emits DeleteStuckServer
        mock_hetzner
            .servers
            .lock()
            .unwrap()
            .push(crate::hetzner::HetznerServer {
                id: 42,
                name: "ci-runner-stuck".into(),
                created,
                labels: std::collections::HashMap::new(),
            });

        // Previous iteration had this server as Provisioning → carry_forward also emits DeleteStuckServer
        let mut manager = NodeManager::new("v1.32.0+k3s1".into(), "10.0.0.1".into());
        manager.nodes.push(node_manager::ManagedNode {
            hetzner_server_id: 42,
            hetzner_server_name: "ci-runner-stuck".into(),
            created_at: created,
            state: node_manager::NodeState::Provisioning,
        });

        run_loop_iteration(
            &mut manager,
            &mock_gitea,
            &mock_hetzner,
            &mock_kube,
            &metrics,
            &config,
        )
        .await
        .unwrap();

        // Should only delete once, not twice
        let delete_calls = mock_hetzner.delete_calls.lock().unwrap();
        assert_eq!(delete_calls.len(), 1);
        assert_eq!(delete_calls[0], 42);
    }
}
