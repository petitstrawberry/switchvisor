{
  description = "Switchvisor Rust development tools for Tegra210 EL2 and host utilities";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
    in {
      devShells = nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rust = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "rust-analyzer" ];
            targets = [ "aarch64-unknown-none-softfloat" ];
          };
        in {
          default = pkgs.mkShell {
            packages = [ rust pkgs.llvmPackages.llvm pkgs.ripgrep pkgs.git pkgs.qemu pkgs.python3 ];
            shellHook = ''
              # Listing Cargo's bin explicitly prevents it from taking priority over Nix tools.
              export PATH="${rust}/bin:$PATH:''${CARGO_HOME:-$HOME/.cargo}/bin"
            '';
          };
        });
    };
}
