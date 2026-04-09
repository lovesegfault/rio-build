# Shared option/systemd boilerplate for the four service modules.
#
# NOT a NixOS module — a function library imported per-module via
# `let rioLib = import ./_common.nix { inherit lib config; }; in`.
# `common.nix` (sibling, no underscore) is the NixOS-module half that
# provides `services.rio.{package,logFormat}`; this file provides the
# helpers each service module composes its config from.
{ lib, config }:
let
  inherit (config.services.rio) package logFormat;
in
{
  # Prometheus metrics listen-address option. Only the default port
  # varies per binary (9090..9093).
  mkMetricsOption =
    port:
    lib.mkOption {
      type = lib.types.str;
      default = "[::]:${toString port}";
      description = "Prometheus metrics listen address (`RIO_METRICS_ADDR`).";
    };

  # systemd.services.<name> attrset with the boilerplate every rio
  # binary shares: multi-user.target, network-online ordering,
  # RIO_LOG_FORMAT, ExecStart from services.rio.package, Restart=
  # on-failure. Callers `// override` (recursiveUpdate would be wrong:
  # `after` is a list and the caller supplies the FULL ordering set —
  # network-online.target is unconditional so it's the literal here,
  # caller appends its extras via `extraAfter`).
  mkRioService =
    {
      binary,
      description,
      environment,
      extraAfter ? [ ],
      serviceConfig ? { },
    }:
    {
      inherit description;
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ] ++ extraAfter;
      wants = [ "network-online.target" ];
      environment = {
        RIO_LOG_FORMAT = logFormat;
      }
      // environment;
      serviceConfig = {
        ExecStart = "${package}/bin/${binary}";
        Restart = "on-failure";
        RestartSec = "5s";
      }
      // serviceConfig;
    };
}
