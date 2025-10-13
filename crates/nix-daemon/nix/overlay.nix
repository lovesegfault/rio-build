# SPDX-License-Identifier: MIT-0
# SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>
# SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>

self: super: {
  nix-web = super.callPackage ../nix-web {};
  nix-supervisor = super.callPackage ../nix-supervisor {};
}
