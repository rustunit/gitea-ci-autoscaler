use prometheus::{
    Encoder, Gauge, GaugeVec, Histogram, HistogramOpts, IntCounter, IntCounterVec, Opts, Registry,
};

#[derive(Clone)]
#[allow(dead_code)]
pub struct Metrics {
    pub desired_nodes: Gauge,
    pub managed_nodes: GaugeVec,
    pub nodes_created_total: IntCounter,
    pub nodes_created_by_placement_total: IntCounterVec,
    pub placement_unavailable_total: IntCounterVec,
    pub nodes_deleted_total: IntCounter,
    pub scale_up_errors_total: IntCounter,
    pub scale_down_errors_total: IntCounter,
    pub runners_deregistered_total: IntCounter,
    pub orphaned_nodes_removed_total: IntCounter,
    pub hetzner_api_duration: Histogram,
    pub gitea_api_duration: Histogram,
    pub k8s_api_duration: Histogram,
    pub loop_duration: Histogram,
    pub linux_jobs_waiting: Gauge,
    pub linux_jobs_running: Gauge,
    pub node_age_seconds: GaugeVec,
    pub node_idle_seconds: GaugeVec,
    pub provisioning_duration: Histogram,
    pub last_success_timestamp: Gauge,
    registry: Registry,
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        let desired_nodes =
            Gauge::new("autoscaler_desired_nodes", "Target node count").expect("metric");
        let managed_nodes = GaugeVec::new(
            Opts::new("autoscaler_managed_nodes", "Per-state node count"),
            &["state"],
        )
        .expect("metric");
        let nodes_created_total =
            IntCounter::new("autoscaler_nodes_created_total", "Servers created").expect("metric");
        let nodes_created_by_placement_total = IntCounterVec::new(
            Opts::new(
                "autoscaler_nodes_created_by_placement_total",
                "Servers created per server type and location",
            ),
            &["server_type", "location"],
        )
        .expect("metric");
        let placement_unavailable_total = IntCounterVec::new(
            Opts::new(
                "autoscaler_placement_unavailable_total",
                "Times Hetzner had no capacity for a server type in a location",
            ),
            &["server_type", "location"],
        )
        .expect("metric");
        let nodes_deleted_total =
            IntCounter::new("autoscaler_nodes_deleted_total", "Servers deleted").expect("metric");
        let scale_up_errors_total =
            IntCounter::new("autoscaler_scale_up_errors_total", "Failed create calls")
                .expect("metric");
        let scale_down_errors_total = IntCounter::new(
            "autoscaler_scale_down_errors_total",
            "Failed teardown operations",
        )
        .expect("metric");
        let runners_deregistered_total = IntCounter::new(
            "autoscaler_runners_deregistered_total",
            "Runners deregistered from Gitea",
        )
        .expect("metric");
        let orphaned_nodes_removed_total = IntCounter::new(
            "autoscaler_orphaned_nodes_removed_total",
            "K8s nodes removed because their Hetzner server was gone",
        )
        .expect("metric");
        let hetzner_api_duration = Histogram::with_opts(HistogramOpts::new(
            "autoscaler_hetzner_api_duration_seconds",
            "Hetzner API latency",
        ))
        .expect("metric");
        let gitea_api_duration = Histogram::with_opts(HistogramOpts::new(
            "autoscaler_gitea_api_duration_seconds",
            "Gitea API latency",
        ))
        .expect("metric");
        let k8s_api_duration = Histogram::with_opts(HistogramOpts::new(
            "autoscaler_k8s_api_duration_seconds",
            "Kubernetes API latency",
        ))
        .expect("metric");
        let loop_duration = Histogram::with_opts(HistogramOpts::new(
            "autoscaler_loop_duration_seconds",
            "Control loop iteration time",
        ))
        .expect("metric");
        let linux_jobs_waiting = Gauge::new(
            "autoscaler_linux_jobs_waiting",
            "Filtered linux jobs waiting",
        )
        .expect("metric");
        let linux_jobs_running = Gauge::new(
            "autoscaler_linux_jobs_running",
            "Filtered linux jobs running",
        )
        .expect("metric");
        let node_age_seconds = GaugeVec::new(
            Opts::new("autoscaler_node_age_seconds", "Per-node age"),
            &["node"],
        )
        .expect("metric");
        let node_idle_seconds = GaugeVec::new(
            Opts::new("autoscaler_node_idle_seconds", "Per-node idle duration"),
            &["node"],
        )
        .expect("metric");
        let provisioning_duration = Histogram::with_opts(
            HistogramOpts::new(
                "autoscaler_provisioning_duration_seconds",
                "Time from server creation to runner registration",
            )
            .buckets(vec![30.0, 60.0, 90.0, 120.0, 180.0, 240.0, 300.0, 600.0]),
        )
        .expect("metric");
        let last_success_timestamp = Gauge::new(
            "autoscaler_last_success_timestamp_seconds",
            "Unix time of the last fully-successful control loop iteration",
        )
        .expect("metric");
        // Seed with startup time so a fresh process isn't flagged stale before its first success.
        last_success_timestamp.set(chrono::Utc::now().timestamp() as f64);

        registry
            .register(Box::new(desired_nodes.clone()))
            .expect("register");
        registry
            .register(Box::new(managed_nodes.clone()))
            .expect("register");
        registry
            .register(Box::new(nodes_created_total.clone()))
            .expect("register");
        registry
            .register(Box::new(nodes_created_by_placement_total.clone()))
            .expect("register");
        registry
            .register(Box::new(placement_unavailable_total.clone()))
            .expect("register");
        registry
            .register(Box::new(nodes_deleted_total.clone()))
            .expect("register");
        registry
            .register(Box::new(scale_up_errors_total.clone()))
            .expect("register");
        registry
            .register(Box::new(scale_down_errors_total.clone()))
            .expect("register");
        registry
            .register(Box::new(runners_deregistered_total.clone()))
            .expect("register");
        registry
            .register(Box::new(orphaned_nodes_removed_total.clone()))
            .expect("register");
        registry
            .register(Box::new(hetzner_api_duration.clone()))
            .expect("register");
        registry
            .register(Box::new(gitea_api_duration.clone()))
            .expect("register");
        registry
            .register(Box::new(k8s_api_duration.clone()))
            .expect("register");
        registry
            .register(Box::new(loop_duration.clone()))
            .expect("register");
        registry
            .register(Box::new(linux_jobs_waiting.clone()))
            .expect("register");
        registry
            .register(Box::new(linux_jobs_running.clone()))
            .expect("register");
        registry
            .register(Box::new(node_age_seconds.clone()))
            .expect("register");
        registry
            .register(Box::new(node_idle_seconds.clone()))
            .expect("register");
        registry
            .register(Box::new(provisioning_duration.clone()))
            .expect("register");
        registry
            .register(Box::new(last_success_timestamp.clone()))
            .expect("register");

        Self {
            desired_nodes,
            managed_nodes,
            nodes_created_total,
            nodes_created_by_placement_total,
            placement_unavailable_total,
            nodes_deleted_total,
            scale_up_errors_total,
            scale_down_errors_total,
            runners_deregistered_total,
            orphaned_nodes_removed_total,
            hetzner_api_duration,
            gitea_api_duration,
            k8s_api_duration,
            loop_duration,
            linux_jobs_waiting,
            linux_jobs_running,
            node_age_seconds,
            node_idle_seconds,
            provisioning_duration,
            last_success_timestamp,
            registry,
        }
    }

    pub async fn push(&self, pushgateway_url: &str) -> anyhow::Result<()> {
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        let encoder = prometheus::TextEncoder::new();
        encoder.encode(&metric_families, &mut buffer)?;

        let client = reqwest::Client::new();
        client
            .post(format!(
                "{}/metrics/job/gitea-ci-autoscaler",
                pushgateway_url
            ))
            .header("Content-Type", "text/plain")
            .body(buffer)
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }

    pub fn set_managed_node_counts(
        &self,
        provisioning: usize,
        busy: usize,
        idle: usize,
        deregistering: usize,
        draining: usize,
        removing: usize,
    ) {
        self.managed_nodes
            .with_label_values(&["provisioning"])
            .set(provisioning as f64);
        self.managed_nodes
            .with_label_values(&["busy"])
            .set(busy as f64);
        self.managed_nodes
            .with_label_values(&["idle"])
            .set(idle as f64);
        self.managed_nodes
            .with_label_values(&["deregistering"])
            .set(deregistering as f64);
        self.managed_nodes
            .with_label_values(&["draining"])
            .set(draining as f64);
        self.managed_nodes
            .with_label_values(&["removing"])
            .set(removing as f64);
    }
}
