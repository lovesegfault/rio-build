# SPDX-License-Identifier: MIT-0
# SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>

with builtins;
with (fromJSON (readFile ../flake.lock)).nodes.nixpkgs.locked;
import (fetchTarball {
  url = "https://github.com/${owner}/${repo}/tarball/${rev}";
  sha256 = narHash;
}) {
  config.allowAliases = false;
}
