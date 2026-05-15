{ pkgs, treefmt-nix }:
(treefmt-nix.lib.evalModule pkgs {
  projectRootFile = "flake.nix";
  programs.nixfmt.enable = true;
  programs.rustfmt.enable = true;
}).config.build.wrapper
