let
  rustOverlay = import (builtins.fetchTarball "https://github.com/oxalica/rust-overlay/archive/master.tar.gz");
  pkgs = import <nixpkgs> {
    overlays = [ rustOverlay ];
  };
  rustToolchain = pkgs.rust-bin.stable.latest.default.override {
    targets = [ "wasm32-wasip1" ];
    extensions = [
      "clippy"
      "rust-analyzer"
      "rust-src"
      "rustfmt"
    ];
  };
in
pkgs.mkShell {
  packages = with pkgs; [
    git
    rustToolchain
  ];

  CARGO_BUILD_TARGET = "wasm32-wasip1";
  RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";

  shellHook = ''
    export CARGO_TARGET_DIR="$PWD/target"
  '';
}
