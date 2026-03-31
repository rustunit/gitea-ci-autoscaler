IMAGE := "ghcr.io/rustunit/gitea-ci-autoscaler:latest"

check:
    cargo fmt -- --check
    cargo sort --check
    cargo c
    cargo t
    cargo clippy --all-features --all-targets -- -D warnings

push-docker:
    docker buildx build --platform linux/amd64 -t {{ IMAGE }} --progress plain --no-cache --push .
