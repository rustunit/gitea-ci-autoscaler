use async_trait::async_trait;
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize)]
struct JobsResponse {
    jobs: Vec<Job>,
}

#[derive(Debug, Deserialize)]
struct RunnersResponse {
    runners: Vec<Runner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Waiting,
    Queued,
    InProgress,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            JobStatus::Waiting => "waiting",
            JobStatus::Queued => "queued",
            JobStatus::InProgress => "in_progress",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Job {
    pub id: u64,
    pub name: String,
    pub labels: Vec<String>,
    pub status: String,
    #[serde(default)]
    pub runner_id: Option<u64>,
    #[serde(default)]
    pub runner_name: Option<String>,
}

impl Job {
    pub fn is_linux(&self) -> bool {
        self.labels.iter().any(|l| l == "linux")
    }
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RunnerLabel {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct Runner {
    pub id: u64,
    pub name: String,
    pub status: String,
    pub busy: bool,
    pub labels: Vec<RunnerLabel>,
}

#[async_trait]
pub trait GiteaClient: Send + Sync {
    async fn list_jobs(&self, status: JobStatus) -> anyhow::Result<Vec<Job>>;
    async fn list_runners(&self) -> anyhow::Result<Vec<Runner>>;
    async fn delete_runner(&self, runner_id: u64) -> anyhow::Result<()>;
}

pub fn filter_linux_jobs(jobs: &[Job]) -> Vec<&Job> {
    jobs.iter().filter(|j| j.is_linux()).collect()
}

// --- Real implementation ---

pub struct RealGiteaClient {
    base_url: String,
    token: String,
    http: reqwest::Client,
}

impl RealGiteaClient {
    pub fn new(base_url: String, token: String) -> Self {
        Self {
            base_url,
            token,
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl GiteaClient for RealGiteaClient {
    async fn list_jobs(&self, status: JobStatus) -> anyhow::Result<Vec<Job>> {
        let mut all_jobs = Vec::new();
        let mut page = 1u32;
        loop {
            let resp: JobsResponse = self
                .http
                .get(format!("{}/api/v1/admin/actions/jobs", self.base_url))
                .query(&[
                    ("status", status.as_str()),
                    ("limit", "50"),
                    ("page", &page.to_string()),
                ])
                .header("Authorization", format!("token {}", self.token))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            let count = resp.jobs.len();
            all_jobs.extend(resp.jobs);
            if count < 50 {
                break;
            }
            page += 1;
        }
        info!(
            status = status.as_str(),
            count = all_jobs.len(),
            "listed gitea jobs"
        );
        Ok(all_jobs)
    }

    async fn list_runners(&self) -> anyhow::Result<Vec<Runner>> {
        let resp: RunnersResponse = self
            .http
            .get(format!("{}/api/v1/admin/actions/runners", self.base_url))
            .header("Authorization", format!("token {}", self.token))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        info!(count = resp.runners.len(), "listed gitea runners");
        Ok(resp.runners)
    }

    async fn delete_runner(&self, runner_id: u64) -> anyhow::Result<()> {
        self.http
            .delete(format!(
                "{}/api/v1/admin/actions/runners/{}",
                self.base_url, runner_id
            ))
            .header("Authorization", format!("token {}", self.token))
            .send()
            .await?
            .error_for_status()?;
        info!(runner_id, "deleted gitea runner");
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn make_job(labels: Vec<&str>, status: &str) -> Job {
        Job {
            id: 1,
            name: "test".to_string(),
            labels: labels.into_iter().map(String::from).collect(),
            status: status.to_string(),
            runner_id: None,
            runner_name: None,
        }
    }

    #[test]
    fn filter_linux_jobs_includes_linux() {
        let jobs = vec![
            make_job(vec!["self-hosted", "linux"], "waiting"),
            make_job(vec!["self-hosted", "macos"], "waiting"),
            make_job(vec!["self-hosted", "linux", "x64"], "waiting"),
        ];
        let filtered = filter_linux_jobs(&jobs);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_linux_jobs_excludes_macos_only() {
        let jobs = vec![make_job(vec!["self-hosted", "macos"], "waiting")];
        let filtered = filter_linux_jobs(&jobs);
        assert!(filtered.is_empty());
    }

    #[test]
    fn filter_linux_jobs_empty() {
        let jobs: Vec<Job> = vec![];
        let filtered = filter_linux_jobs(&jobs);
        assert!(filtered.is_empty());
    }
}
