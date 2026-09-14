use std::{fs, path::PathBuf, process::Command, time::SystemTime};

struct Scratch(PathBuf);
impl Scratch {
    fn new(name: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("switchvisor-{name}-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn pack(&self) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_switchvisor-tool"))
            .arg("pack-script")
            .arg(self.0.join("boot.cmd"))
            .arg(self.0.join("boot.scr"))
            .output()
            .unwrap()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn script_matches_native_legacy_layout_and_independent_crc_golden() {
    let scratch = Scratch::new("script-golden");
    fs::write(scratch.0.join("boot.cmd"), b"echo hello\n").unwrap();
    assert!(scratch.pack().status.success());
    // Linux/ARM/script/none, zero timestamp, one script component. CRCs calculated with zlib.
    let hex = concat!(
        "2705195643eb50a6000000000000001300000000000000003db55e6f05020600",
        "5377697463687669736f7220626f6f7420736372697074000000000000000000",
        "0000000b000000006563686f2068656c6c6f0a"
    );
    let expected: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    assert_eq!(fs::read(scratch.0.join("boot.scr")).unwrap(), expected);
}

#[test]
fn invalid_scripts_and_existing_images_are_rejected_without_overwrite() {
    let scratch = Scratch::new("script-rejection");
    for source in [b"".as_slice(), b"echo\0oops", b"\xff"] {
        fs::write(scratch.0.join("boot.cmd"), source).unwrap();
        assert!(!scratch.pack().status.success());
        assert!(!scratch.0.join("boot.scr").exists());
    }
    fs::write(scratch.0.join("boot.cmd"), b"echo hello\n").unwrap();
    fs::write(scratch.0.join("boot.scr"), b"keep existing image").unwrap();
    assert!(!scratch.pack().status.success());
    assert_eq!(
        fs::read(scratch.0.join("boot.scr")).unwrap(),
        b"keep existing image"
    );
}
