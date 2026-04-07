# Vendored upstream Kubernetes manifests

Manifests here are applied by `xtask k8s deploy` BEFORE the rio helm
chart. They are NOT helm subcharts because either (a) the upstream
project doesn't publish a chart to a registry, or (b) the install
ordering doesn't fit subchart semantics.

(Currently empty — security-profiles-operator was removed once both
EKS and k3s VM tests deliver seccomp profiles via systemd-tmpfiles
on NixOS nodes; see ADR-021.)
