{
  description = "Offline block-level deduplication for ZFS via FICLONERANGE";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      treefmt-nix,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = eachSystem (pkgs: rec {
        zfs-dedup = pkgs.callPackage ./nix/package.nix { };
        default = zfs-dedup;
      });

      devShells = eachSystem (pkgs: {
        default = pkgs.callPackage ./nix/shell.nix {
          zfs-dedup = pkgs.callPackage ./nix/package.nix { };
        };
      });

      formatter = eachSystem (
        pkgs: (import ./nix/fmt.nix { inherit pkgs treefmt-nix; }).config.build.wrapper
      );

      checks = eachSystem (
        pkgs:
        let
          zfs-dedup = pkgs.callPackage ./nix/package.nix { };
          treefmt = import ./nix/fmt.nix { inherit pkgs treefmt-nix; };
        in
        {
          inherit zfs-dedup;
          clippy = zfs-dedup.overrideAttrs (old: {
            pname = "zfs-dedup-clippy";
            nativeBuildInputs = old.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = "cargo clippy --all-targets -- -D warnings";
            doCheck = false;
            installPhase = "touch $out";
          });
          formatting = treefmt.config.build.check self;
        }
      );
    };
}
