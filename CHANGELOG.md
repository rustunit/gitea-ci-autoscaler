# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0]

### Added

- `K3S_AGENT_ARGS` environment variable to customize the arguments appended to
  the `k3s agent` command in each node's cloud-init. Use it to set node labels,
  taints, etc. (e.g. `--node-taint=ci=true:NoSchedule`) without changing the
  code. Defaults to the previously hardcoded node labels.

## [0.1.0]

- Initial release.
