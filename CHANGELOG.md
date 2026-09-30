# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.1]

### Fixed

- A k8s node whose Hetzner server is gone (deleted by hand, or lost by Hetzner) is
  removed from the cluster and its runner deregistered from Gitea. Such a node used to
  stay NotReady forever. It is removed once it has been NotReady with no matching
  server for 30 seconds; a Ready node is never touched.

### Added

- Metric `autoscaler_orphaned_nodes_removed_total`.

## [0.4.0]

### Added

- Fallback server types and locations: `HETZNER_SERVER_TYPE` and `HETZNER_LOCATION`
  accept comma-separated lists. Every location is tried for the first type, then for
  the next. A single value behaves as before.
- A placement Hetzner has no capacity for is skipped for `PLACEMENT_COOLDOWN_SECS`
  (default 300) instead of being retried on every loop iteration.
- Metrics `autoscaler_placement_unavailable_total` and
  `autoscaler_nodes_created_by_placement_total`, both labelled by server type and location.
- The `created hetzner server` log line carries `server_type` and `location`.

### Fixed

- A server Hetzner accepts but never places (it disappears from the API without an
  error) is detected after 20 seconds and replaced from the next placement. It used
  to count as provisioning until `PROVISIONING_TIMEOUT_SECS` ran out.

## [0.3.2]

### Fixed

- Stuck servers are deleted once per iteration instead of twice when more than
  one is stuck at the same time (the second call failed with a Hetzner 404).

## [0.3.1]

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
