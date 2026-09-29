// get.k3s.io can go down on its own (Cloudflare 1101); the same script is served from the
// release tag on GitHub. Download to a file so a failed or partial fetch never reaches `sh`.
pub fn render(master_ip: &str, join_token: &str, k3s_version: &str, agent_args: &str) -> String {
    format!(
        r#"#cloud-config
runcmd:
  - curl -sfL -o /tmp/k3s-install.sh https://get.k3s.io || curl -sfL -o /tmp/k3s-install.sh https://raw.githubusercontent.com/k3s-io/k3s/{k3s_version}/install.sh
  - K3S_URL=https://{master_ip}:6443 K3S_TOKEN={join_token} INSTALL_K3S_VERSION={k3s_version} sh /tmp/k3s-install.sh agent {agent_args}"#
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cloud_init_render() {
        let result = render(
            "10.0.0.1",
            "my-token",
            "v1.32.11+k3s1",
            "--node-label=managed-by=gitea-ci-autoscaler --node-taint=ci=true:NoSchedule",
        );

        assert!(result.starts_with("#cloud-config"));
        assert!(result.contains("K3S_URL=https://10.0.0.1:6443"));
        assert!(result.contains("K3S_TOKEN=my-token"));
        assert!(result.contains("INSTALL_K3S_VERSION=v1.32.11+k3s1"));
        assert!(result.contains(
            "curl -sfL -o /tmp/k3s-install.sh https://get.k3s.io || curl -sfL -o /tmp/k3s-install.sh https://raw.githubusercontent.com/k3s-io/k3s/v1.32.11+k3s1/install.sh"
        ));
        assert!(result.contains("sh /tmp/k3s-install.sh agent --node-label=managed-by=gitea-ci-autoscaler --node-taint=ci=true:NoSchedule"));
    }
}
