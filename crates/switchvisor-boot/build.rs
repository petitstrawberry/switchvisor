use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=boot.ld");
    println!("cargo:rerun-if-changed=src/entry.S");
    println!("cargo:rerun-if-env-changed=SWITCHVISOR_LINK_BASE");
    if env::var_os("CARGO_FEATURE_BAREMETAL").is_some() {
        assert_eq!(
            env::var("TARGET").unwrap(),
            "aarch64-unknown-none-softfloat",
            "use cargo build-boot in nix develop"
        );
        let script = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("boot.ld");
        println!("cargo:rustc-link-arg=-T{}", script.display());
        println!("cargo:rustc-link-arg=--build-id=none");
        if let Ok(base) = env::var("SWITCHVISOR_LINK_BASE") {
            assert_eq!(
                base, "0xB0000000",
                "payload launcher uses a fixed resident base"
            );
            println!("cargo:rustc-link-arg=--defsym=SWITCHVISOR_LINK_BASE={base}");
        }
    }
}
