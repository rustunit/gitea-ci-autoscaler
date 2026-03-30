pub fn render(master_ip: &str, join_token: &str, k3s_version: &str) -> String {
    format!(
        r#"#cloud-config
runcmd:
  - curl -sfL https://get.k3s.io | K3S_URL=https://{master_ip}:6443 K3S_TOKEN={join_token} INSTALL_K3S_VERSION={k3s_version} sh -s - agent --node-label=managed-by=gitea-ci-autoscaler --node-label=node-role=ci-runner"#
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cloud_init_render() {
        let result = render("10.0.0.1", "my-token", "v1.32.11+k3s1");

        assert!(result.starts_with("#cloud-config"));
        assert!(result.contains("K3S_URL=https://10.0.0.1:6443"));
        assert!(result.contains("K3S_TOKEN=my-token"));
        assert!(result.contains("INSTALL_K3S_VERSION=v1.32.11+k3s1"));
        assert!(result.contains("--node-label=managed-by=gitea-ci-autoscaler"));
        assert!(result.contains("--node-label=node-role=ci-runner"));
    }
}
