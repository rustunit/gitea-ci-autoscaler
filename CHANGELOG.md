# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- New nodes fall back to the version-tagged k3s `install.sh` on GitHub when
  `get.k3s.io` is unavailable, instead of never joining the cluster.

## [0.3.0]

### Added

- added `autoscaler_last_success_timestamp_seconds` metric

## [0.2.1]

### Fixed

- Control loop no longer fails with `missing field 'datacenter'`.
- Server listings are now paginated.
- Failed Hetzner API calls now report the error message.

### Changed

- Replaced the `hcloud` dependency with direct HTTP calls that deserialize only
  the server fields we use, so future additions or removals in the Hetzner API
  cannot break deserialization again.

## [0.2.0]

### Added

- `K3S_AGENT_ARGS` environment variable to customize the arguments appended to
  the `k3s agent` command in each node's cloud-init. Use it to set node labels,
  taints, etc. (e.g. `--node-taint=ci=true:NoSchedule`) without changing the
  code. Defaults to the previously hardcoded node labels.

## [0.1.0]

- Initial release.
