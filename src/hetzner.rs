use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tracing::{info, warn};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct HetznerServer {
    pub id: i64,
    pub name: String,
    pub created: DateTime<Utc>,
    pub labels: std::collections::HashMap<String, String>,
}

#[async_trait]
pub trait HetznerClient: Send + Sync {
    async fn create_server(&self, name: &str, cloud_init: &str) -> anyhow::Result<HetznerServer>;
    async fn list_servers(&self) -> anyhow::Result<Vec<HetznerServer>>;
    async fn delete_server(&self, server_id: i64) -> anyhow::Result<()>;
}

// --- Real implementation ---

pub struct RealHetznerClient {
    config: hcloud::apis::configuration::Configuration,
    server_type: String,
    location: String,
    image: String,
    ssh_keys: Vec<String>,
}

impl RealHetznerClient {
    pub async fn new(
        api_token: String,
        server_type: String,
        location: String,
        image: String,
    ) -> anyhow::Result<Self> {
        let mut config = hcloud::apis::configuration::Configuration::new();
        config.bearer_access_token = Some(api_token);

        let params = hcloud::apis::ssh_keys_api::ListSshKeysParams::default();
        let response = hcloud::apis::ssh_keys_api::list_ssh_keys(&config, params).await?;
        let ssh_keys: Vec<String> = response.ssh_keys.iter().map(|k| k.name.clone()).collect();

        if ssh_keys.is_empty() {
            warn!(
                "no ssh keys found in hetzner project - servers will be created with root passwords"
            );
        } else {
            info!(count = ssh_keys.len(), names = ?ssh_keys, "loaded ssh keys from hetzner");
        }

        Ok(Self {
            config,
            server_type,
            location,
            image,
            ssh_keys,
        })
    }
}

const MANAGED_BY_LABEL: &str = "managed-by";
const MANAGED_BY_VALUE: &str = "gitea-ci-autoscaler";

#[async_trait]
impl HetznerClient for RealHetznerClient {
    async fn create_server(&self, name: &str, cloud_init: &str) -> anyhow::Result<HetznerServer> {
        let mut labels = std::collections::HashMap::new();
        labels.insert(MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string());

        let params = hcloud::apis::servers_api::CreateServerParams {
            create_server_request: Some(hcloud::models::CreateServerRequest {
                name: name.to_string(),
                server_type: self.server_type.clone(),
                image: self.image.clone(),
                location: Some(self.location.clone()),
                labels: Some(labels),
                user_data: Some(cloud_init.to_string()),
                ssh_keys: Some(self.ssh_keys.clone()),
                automount: Some(false),
                start_after_create: Some(true),
                ..Default::default()
            }),
        };

        let response = hcloud::apis::servers_api::create_server(&self.config, params).await?;

        let server = response.server;
        info!(server_id = server.id, server_name = %server.name, "created hetzner server");
        Ok(HetznerServer {
            id: server.id,
            name: server.name,
            created: server.created.parse::<DateTime<Utc>>()?,
            labels: server.labels,
        })
    }

    async fn list_servers(&self) -> anyhow::Result<Vec<HetznerServer>> {
        let params = hcloud::apis::servers_api::ListServersParams {
            label_selector: Some(format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE}")),
            ..Default::default()
        };

        let response = hcloud::apis::servers_api::list_servers(&self.config, params).await?;

        let servers = response
            .servers
            .into_iter()
            .map(|s| {
                Ok(HetznerServer {
                    id: s.id,
                    name: s.name,
                    created: s.created.parse::<DateTime<Utc>>()?,
                    labels: s.labels,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        info!(count = servers.len(), "listed hetzner servers");
        Ok(servers)
    }

    async fn delete_server(&self, server_id: i64) -> anyhow::Result<()> {
        let params = hcloud::apis::servers_api::DeleteServerParams { id: server_id };
        hcloud::apis::servers_api::delete_server(&self.config, params).await?;
        info!(server_id, "deleted hetzner server");
        Ok(())
    }
}
