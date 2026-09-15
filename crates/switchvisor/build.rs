use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/arch/aarch64/entry.S");
    if env::var_os("CARGO_FEATURE_BAREMETAL").is_some() {
        assert_eq!(
            env::var("TARGET").unwrap(),
            "aarch64-unknown-none-softfloat",
            "use cargo build-hv in nix develop"
        );
        let script = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("linker.ld");
        println!(
            "cargo:rustc-link-arg-bin=switchvisor=-T{}",
            script.display()
        );
        println!("cargo:rustc-link-arg-bin=switchvisor=--build-id=none");
    }
}
