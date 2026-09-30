use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use tracing::{info, warn};

const API_BASE: &str = "https://api.hetzner.cloud/v1";
const PER_PAGE: u32 = 50;

// Hand-rolled instead of a generated client: generated models mark every documented
// field required, so Hetzner dropping one (e.g. `datacenter`, removed 2026-07-01)
// breaks deserialization. Declaring only what we read keeps us immune.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct HetznerServer {
    pub id: i64,
    pub name: String,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub labels: HashMap<String, String>,
}

/// One server type in one location, tried in config order when creating a server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Placement {
    pub server_type: String,
    pub location: String,
}

#[derive(Debug, Error)]
pub enum CreateError {
    /// Hetzner has no capacity for this placement right now; another one may work.
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[async_trait]
pub trait HetznerClient: Send + Sync {
    async fn create_server(
        &self,
        name: &str,
        cloud_init: &str,
        placement: &Placement,
    ) -> Result<HetznerServer, CreateError>;
    async fn list_servers(&self) -> anyhow::Result<Vec<HetznerServer>>;
    /// False once Hetzner no longer knows the server (deleted, or never placed).
    async fn server_exists(&self, server_id: i64) -> anyhow::Result<bool>;
    async fn delete_server(&self, server_id: i64) -> anyhow::Result<()>;
}

// --- Real implementation ---

#[derive(Debug, Deserialize)]
struct ServersResponse {
    servers: Vec<HetznerServer>,
}

#[derive(Debug, Deserialize)]
struct CreateServerResponse {
    server: HetznerServer,
}

#[derive(Debug, Deserialize)]
struct SshKeysResponse {
    ssh_keys: Vec<SshKey>,
}

#[derive(Debug, Deserialize)]
struct SshKey {
    name: String,
}

#[derive(Debug, Serialize)]
struct CreateServerRequest<'a> {
    name: &'a str,
    server_type: &'a str,
    image: &'a str,
    location: &'a str,
    labels: &'a HashMap<String, String>,
    user_data: &'a str,
    ssh_keys: &'a [String],
    automount: bool,
    start_after_create: bool,
}

pub struct RealHetznerClient {
    token: String,
    http: reqwest::Client,
    image: String,
    ssh_keys: Vec<String>,
}

impl RealHetznerClient {
    pub async fn new(api_token: String, image: String) -> anyhow::Result<Self> {
        let mut client = Self {
            token: api_token,
            http: reqwest::Client::new(),
            image,
            ssh_keys: Vec::new(),
        };

        let resp: SshKeysResponse = client.get("ssh_keys", &[]).await?;
        client.ssh_keys = resp.ssh_keys.into_iter().map(|k| k.name).collect();

        if client.ssh_keys.is_empty() {
            warn!(
                "no ssh keys found in hetzner project - servers will be created with root passwords"
            );
        } else {
            info!(count = client.ssh_keys.len(), names = ?client.ssh_keys, "loaded ssh keys from hetzner");
        }

        Ok(client)
    }

    async fn get<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> anyhow::Result<T> {
        let resp = self
            .http
            .get(format!("{API_BASE}/{path}"))
            .query(query)
            .bearer_auth(&self.token)
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }
}

#[derive(Debug, Error)]
#[error("hetzner api returned {status}: {body}")]
struct ApiError {
    status: reqwest::StatusCode,
    body: String,
}

impl ApiError {
    /// The machine-readable `error.code` from Hetzner's error body.
    fn code(&self) -> Option<String> {
        #[derive(Deserialize)]
        struct Body {
            error: Inner,
        }
        #[derive(Deserialize)]
        struct Inner {
            code: String,
        }
        serde_json::from_str::<Body>(&self.body)
            .ok()
            .map(|b| b.error.code)
    }

    /// Out of stock, or the type is not offered in the location.
    fn is_unavailable(&self) -> bool {
        matches!(
            self.code().as_deref(),
            Some("resource_unavailable" | "placement_error")
        )
    }
}

/// Hetzner puts the failure reason in the body, which `error_for_status` would drop.
async fn check(resp: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(ApiError { status, body })
}

const MANAGED_BY_LABEL: &str = "managed-by";
const MANAGED_BY_VALUE: &str = "gitea-ci-autoscaler";

#[async_trait]
impl HetznerClient for RealHetznerClient {
    async fn create_server(
        &self,
        name: &str,
        cloud_init: &str,
        placement: &Placement,
    ) -> Result<HetznerServer, CreateError> {
        let mut labels = HashMap::new();
        labels.insert(MANAGED_BY_LABEL.to_string(), MANAGED_BY_VALUE.to_string());

        let body = CreateServerRequest {
            name,
            server_type: &placement.server_type,
            image: &self.image,
            location: &placement.location,
            labels: &labels,
            user_data: cloud_init,
            ssh_keys: &self.ssh_keys,
            automount: false,
            start_after_create: true,
        };

        let resp = self
            .http
            .post(format!("{API_BASE}/servers"))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .map_err(anyhow::Error::from)?;
        let resp = match check(resp).await {
            Ok(resp) => resp,
            Err(e) if e.is_unavailable() => return Err(CreateError::Unavailable(e.to_string())),
            Err(e) => return Err(CreateError::Other(e.into())),
        };
        let resp: CreateServerResponse = resp.json().await.map_err(anyhow::Error::from)?;

        let server = resp.server;
        info!(
            server_id = server.id,
            server_name = %server.name,
            server_type = %placement.server_type,
            location = %placement.location,
            "created hetzner server"
        );
        Ok(server)
    }

    async fn list_servers(&self) -> anyhow::Result<Vec<HetznerServer>> {
        let label_selector = format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE}");
        let mut servers = Vec::new();
        let mut page = 1u32;
        loop {
            let resp: ServersResponse = self
                .get(
                    "servers",
                    &[
                        ("label_selector", label_selector.as_str()),
                        ("per_page", &PER_PAGE.to_string()),
                        ("page", &page.to_string()),
                    ],
                )
                .await?;

            let count = resp.servers.len() as u32;
            servers.extend(resp.servers);
            if count < PER_PAGE {
                break;
            }
            page += 1;
        }

        info!(count = servers.len(), "listed hetzner servers");
        Ok(servers)
    }

    async fn server_exists(&self, server_id: i64) -> anyhow::Result<bool> {
        let resp = self
            .http
            .get(format!("{API_BASE}/servers/{server_id}"))
            .bearer_auth(&self.token)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        check(resp).await?;
        Ok(true)
    }

    async fn delete_server(&self, server_id: i64) -> anyhow::Result<()> {
        let resp = self
            .http
            .delete(format!("{API_BASE}/servers/{server_id}"))
            .bearer_auth(&self.token)
            .send()
            .await?;
        check(resp).await?;
        info!(server_id, "deleted hetzner server");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A real 2026-07 server payload: no `datacenter`, plus fields we don't model.
    #[test]
    fn deserializes_server_without_datacenter() {
        let json = r#"{
            "servers": [{
                "id": 42,
                "name": "ci-runner-abc",
                "created": "2026-07-27T08:00:00+00:00",
                "labels": { "managed-by": "gitea-ci-autoscaler" },
                "status": "running",
                "location": { "id": 1, "name": "fsn1" },
                "public_net": { "ipv4": { "ip": "1.2.3.4" } }
            }]
        }"#;
        let resp: ServersResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.servers.len(), 1);
        assert_eq!(resp.servers[0].id, 42);
        assert_eq!(resp.servers[0].name, "ci-runner-abc");
        assert_eq!(
            resp.servers[0].labels.get("managed-by").map(String::as_str),
            Some("gitea-ci-autoscaler")
        );
    }

    fn api_error(status: u16, body: &str) -> ApiError {
        ApiError {
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            body: body.to_string(),
        }
    }

    /// The body Hetzner returned on 2026-09-30 when cx53 ran out of stock in fsn1.
    #[test]
    fn out_of_stock_is_unavailable() {
        let e = api_error(
            412,
            r#"{"error": {"code": "resource_unavailable", "message": "error during placement", "details": {}}}"#,
        );
        assert_eq!(e.code().as_deref(), Some("resource_unavailable"));
        assert!(e.is_unavailable());
    }

    #[test]
    fn other_api_errors_are_not_unavailable() {
        let e = api_error(
            429,
            r#"{"error": {"code": "rate_limit_exceeded", "message": "limit reached"}}"#,
        );
        assert!(!e.is_unavailable());
        assert!(!api_error(504, "upstream request timeout").is_unavailable());
    }
}
