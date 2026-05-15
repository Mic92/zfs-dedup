{
  description = "Offline block-level deduplication for ZFS via FICLONERANGE";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      imports = [ inputs.treefmt-nix.flakeModule ];

      perSystem =
        { pkgs, self', ... }:
        {
          packages.default = pkgs.rustPlatform.buildRustPackage {
            pname = "zfs-dedup";
            version = "0.1.0";
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
          };

          devShells.default = pkgs.mkShell {
            inputsFrom = [ self'.packages.default ];
            packages = with pkgs; [
              rustc
              cargo
              clippy
              rustfmt
              rust-analyzer
              cargo-watch
            ];
            RUST_BACKTRACE = "1";
          };

          treefmt = {
            projectRootFile = "flake.nix";
            programs.nixfmt.enable = true;
            programs.rustfmt.enable = true;
          };
        };
    };
}
