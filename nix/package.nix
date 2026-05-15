{ rustPlatform }:
rustPlatform.buildRustPackage {
  pname = "zfs-dedup";
  version = "0.1.0";
  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;
}
