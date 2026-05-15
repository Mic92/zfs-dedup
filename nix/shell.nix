{
  mkShell,
  zfs-dedup,
  rustc,
  cargo,
  clippy,
  rustfmt,
  rust-analyzer,
  cargo-watch,
}:
mkShell {
  inputsFrom = [ zfs-dedup ];
  packages = [
    rustc
    cargo
    clippy
    rustfmt
    rust-analyzer
    cargo-watch
  ];
  RUST_BACKTRACE = "1";
}
