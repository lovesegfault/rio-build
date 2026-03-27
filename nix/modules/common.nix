# Shared options for all rio services.
#
# Each service module (store.nix, scheduler.nix, gateway.nix, worker.nix)
# imports this to get `services.rio.package` and `services.rio.logFormat`.
{ lib, ... }:
{
  options.services.rio = {
    package = lib.mkOption {
      type = lib.types.package;
      description = ''
        The rio-build workspace package providing all binaries
        (rio-store, rio-scheduler, rio-gateway, rio-builder).
        Typically the flake's `packages.default` (Crane-built workspace).
      '';
    };

    logFormat = lib.mkOption {
      type = lib.types.enum [
        "json"
        "pretty"
      ];
      default = "json";
      description = ''
        Log format for all rio services (`RIO_LOG_FORMAT`).
        `json` for structured logging (production), `pretty` for development.
      '';
    };
  };
}
