# nix-build-style fixture for the rio-eval-smoke check (run 6): an
# auto-called top-level function whose arguments come from --argstr
# (`name`) and --arg (`tagged`, a Nix expression), and whose source
# comes through the angle-bracket lookup path (`<probe>`, resolved via
# -I). The drv name encodes both overrides so a single basename check
# proves the autoArgs and the lookup path were both honored.
{
  name ? "smoke-args-default",
  tagged ? false,
  probe ? <probe>,
}:
derivation {
  name = name + (if tagged then "-tagged" else "");
  inherit probe;
  system = "x86_64-linux";
  builder = "/bin/sh";
  args = [
    "-c"
    "cat $probe/marker.txt > $out"
  ];
}
