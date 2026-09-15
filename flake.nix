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
          nxboot = pkgs.stdenv.mkDerivation rec {
            pname = "nxboot";
            version = "unstable-2025-05-26";
            src = pkgs.fetchFromGitHub {
              owner = "mologie";
              repo = "nxboot";
              rev = "5ba7bce91840e41fb2bdfeba47482c95ccdea7e9";
              hash = "sha256-O3Jm62B64pN2+Od2PhnnFT72oTh+ulXJs0wHXI3FkIk=";
            };
            strictDeps = true;
            dontConfigure = true;
            buildPhase = ''
              runHook preBuild
              $CC NXBootCmd/*.m NXBootKit/*.m \
                -DNXBOOT_VERSION='"${version}"' -DNXBOOT_BUILDNO=1 \
                -I. -INXBootKit -std=gnu11 -fobjc-arc -fobjc-weak -fmodules \
                -fvisibility=hidden -Wall -O2 \
                -framework CoreFoundation -framework Foundation -framework IOKit \
                -Wl,-sectcreate,__TEXT,__intermezzo,Shared/intermezzo.bin \
                -o nxboot-cli
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              install -Dm755 nxboot-cli $out/bin/nxboot
              runHook postInstall
            '';
            meta = {
              description = "Tegra X1 RCM payload launcher for macOS";
              homepage = "https://github.com/mologie/nxboot";
              license = pkgs.lib.licenses.gpl3Only;
              mainProgram = "nxboot";
              platforms = pkgs.lib.platforms.darwin;
            };
          };
        in {
          default = pkgs.mkShell {
            packages = [
              rust
              pkgs.llvmPackages.llvm
              pkgs.ripgrep
              pkgs.git
              pkgs.qemu
              pkgs.python3
              pkgs.dtc
              pkgs.minicom
            ] ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin [ nxboot ];
            shellHook = ''
              # Listing Cargo's bin explicitly prevents it from taking priority over Nix tools.
              export PATH="${rust}/bin:$PATH:''${CARGO_HOME:-$HOME/.cargo}/bin"
            '';
          };
        });
    };
}
