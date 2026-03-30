use std::env;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing required environment variable: {0}")]
    MissingEnvVar(String),
    #[error("invalid value for {0}: {1}")]
    InvalidValue(String, String),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub poll_interval_secs: u64,
    pub max_nodes: usize,
    pub idle_timeout_mins: u64,
    pub billing_window_mins: u64,
    pub provisioning_timeout_secs: u64,
    pub hetzner_server_type: String,
    pub hetzner_location: String,
    pub hetzner_image: String,
    pub hetzner_api_token: String,
    pub cluster_secret: String,
    pub gitea_api_url: String,
    pub gitea_admin_token: String,
    pub pushgateway_url: String,
    pub runner_namespace: String,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            poll_interval_secs: parse_env_or("POLL_INTERVAL_SECS", 5)?,
            max_nodes: parse_env_or("MAX_NODES", 5)?,
            idle_timeout_mins: parse_env_or("IDLE_TIMEOUT_MINS", 5)?,
            billing_window_mins: parse_env_or("BILLING_WINDOW_MINS", 5)?,
            provisioning_timeout_secs: parse_env_or("PROVISIONING_TIMEOUT_SECS", 600)?,
            hetzner_server_type: env_or("HETZNER_SERVER_TYPE", "ccx33"),
            hetzner_location: env_or("HETZNER_LOCATION", "nbg1"),
            hetzner_image: env_or("HETZNER_IMAGE", "ubuntu-24.04"),
            hetzner_api_token: required_env("HETZNER_API_TOKEN")?,
            cluster_secret: required_env("CLUSTER_SECRET")?,
            gitea_api_url: required_env("GITEA_API_URL")?,
            gitea_admin_token: required_env("GITEA_ADMIN_TOKEN")?,
            pushgateway_url: required_env("PUSHGATEWAY_URL")?,
            runner_namespace: env_or("RUNNER_NAMESPACE", "gitea-runners"),
        })
    }
}

fn required_env(key: &str) -> Result<String, ConfigError> {
    env::var(key).map_err(|_| ConfigError::MissingEnvVar(key.to_string()))
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn parse_env_or<T: std::str::FromStr>(key: &str, default: T) -> Result<T, ConfigError> {
    match env::var(key) {
        Ok(val) => val
            .parse()
            .map_err(|_| ConfigError::InvalidValue(key.to_string(), val)),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialize env-mutating tests
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        for key in [
            "POLL_INTERVAL_SECS",
            "MAX_NODES",
            "IDLE_TIMEOUT_MINS",
            "BILLING_WINDOW_MINS",
            "PROVISIONING_TIMEOUT_SECS",
            "HETZNER_SERVER_TYPE",
            "HETZNER_LOCATION",
            "HETZNER_IMAGE",
            "HETZNER_API_TOKEN",
            "CLUSTER_SECRET",
            "GITEA_API_URL",
            "GITEA_ADMIN_TOKEN",
            "PUSHGATEWAY_URL",
            "RUNNER_NAMESPACE",
        ] {
            unsafe { env::remove_var(key) };
        }
    }

    fn set_required_env() {
        unsafe {
            env::set_var("HETZNER_API_TOKEN", "hetzner-token");
            env::set_var("CLUSTER_SECRET", "k3s-token");
            env::set_var("GITEA_ADMIN_TOKEN", "gitea-token");
            env::set_var("GITEA_API_URL", "http://gitea.example.com");
            env::set_var("PUSHGATEWAY_URL", "http://pushgateway.example.com");
        }
    }

    #[test]
    fn config_from_env_defaults() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_env();
        set_required_env();

        let config = Config::from_env().unwrap();
        assert_eq!(config.poll_interval_secs, 5);
        assert_eq!(config.max_nodes, 5);
        assert_eq!(config.idle_timeout_mins, 5);
        assert_eq!(config.billing_window_mins, 5);
        assert_eq!(config.provisioning_timeout_secs, 600);
        assert_eq!(config.hetzner_server_type, "ccx33");
        assert_eq!(config.hetzner_location, "nbg1");
        assert_eq!(config.hetzner_image, "ubuntu-24.04");
        assert_eq!(config.gitea_api_url, "http://gitea.example.com");
        assert_eq!(config.runner_namespace, "gitea-runners");
    }

    #[test]
    fn config_from_env_custom_values() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_env();
        set_required_env();
        unsafe {
            env::set_var("POLL_INTERVAL_SECS", "10");
            env::set_var("MAX_NODES", "3");
            env::set_var("IDLE_TIMEOUT_MINS", "10");
            env::set_var("HETZNER_SERVER_TYPE", "cx22");
        }

        let config = Config::from_env().unwrap();
        assert_eq!(config.poll_interval_secs, 10);
        assert_eq!(config.max_nodes, 3);
        assert_eq!(config.idle_timeout_mins, 10);
        assert_eq!(config.hetzner_server_type, "cx22");
    }

    #[test]
    fn config_missing_required_var() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_env();
        // Don't set required vars

        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("HETZNER_API_TOKEN"));
    }

    #[test]
    fn config_invalid_numeric_value() {
        let _lock = ENV_LOCK.lock().unwrap();
        clear_env();
        set_required_env();
        unsafe {
            env::set_var("MAX_NODES", "not_a_number");
        }

        let err = Config::from_env().unwrap_err();
        assert!(err.to_string().contains("MAX_NODES"));
    }
}
