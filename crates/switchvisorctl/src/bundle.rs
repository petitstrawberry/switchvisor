use serde::Deserialize;
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};
use switchvisor::{
    loader::{BundleDescriptor, ImageDescriptor, MAX_IMAGES, PRESERVE_BOOT_ARGS},
    payload::Crc32,
};

#[derive(Debug)]
pub struct PreparedImage {
    pub path: PathBuf,
    pub descriptor: ImageDescriptor,
}

#[derive(Debug)]
pub struct PreparedBundle {
    pub descriptor: BundleDescriptor,
    pub images: Vec<PreparedImage>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ManifestNumber {
    Integer(u64),
    Text(String),
}

impl ManifestNumber {
    fn value(&self) -> Result<u64, String> {
        match self {
            Self::Integer(value) => Ok(*value),
            Self::Text(value) => number(value),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestImage {
    path: PathBuf,
    address: ManifestNumber,
    #[serde(default)]
    runtime_size: Option<ManifestNumber>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    entry: ManifestNumber,
    #[serde(default)]
    preserve_boot_args: Option<bool>,
    #[serde(default)]
    registers: Option<Vec<ManifestNumber>>,
    images: Vec<ManifestImage>,
}

fn number(value: &str) -> Result<u64, String> {
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => value.parse(),
    }
    .map_err(|error| format!("invalid number {value:?}: {error}"))
}

fn file_crc(path: &Path) -> Result<(u64, u32), String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut buffer = [0; 64 * 1024];
    let mut length = 0u64;
    let mut crc = Crc32::new();
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        length = length
            .checked_add(count as u64)
            .ok_or_else(|| format!("{} is too large", path.display()))?;
        crc.update(&buffer[..count]);
    }
    Ok((length, crc.finish()))
}

fn overlaps(left: ImageDescriptor, right: ImageDescriptor) -> bool {
    let left_end = left.address + left.runtime_size;
    let right_end = right.address + right.runtime_size;
    left.address < right_end && right.address < left_end
}

fn registers(manifest: &Manifest) -> Result<(u32, [u64; 8]), String> {
    let Some(values) = &manifest.registers else {
        return Ok((
            if manifest.preserve_boot_args.unwrap_or(true) {
                PRESERVE_BOOT_ARGS
            } else {
                0
            },
            [0; 8],
        ));
    };
    if manifest.preserve_boot_args == Some(true) {
        return Err("registers cannot be combined with preserve_boot_args=true".into());
    }
    if values.len() != 8 {
        return Err("registers must contain exactly x0 through x7".into());
    }
    let mut registers = [0; 8];
    for (output, input) in registers.iter_mut().zip(values) {
        *output = input.value()?;
    }
    Ok((0, registers))
}

impl PreparedBundle {
    pub fn from_manifest(input: &Path) -> Result<Self, String> {
        let manifest_path = if input.is_dir() {
            input.join("bundle.json")
        } else {
            input.to_path_buf()
        };
        let bytes = fs::read(&manifest_path)
            .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|error| format!("{}: {error}", manifest_path.display()))?;
        if manifest.version != 1 {
            return Err(format!(
                "{}: unsupported bundle version {}",
                manifest_path.display(),
                manifest.version
            ));
        }
        if manifest.images.is_empty() || manifest.images.len() > MAX_IMAGES {
            return Err(format!("bundle must contain 1 to {MAX_IMAGES} images"));
        }
        let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        let mut images: Vec<PreparedImage> = Vec::with_capacity(manifest.images.len());
        for image in &manifest.images {
            let path = if image.path.is_absolute() {
                image.path.clone()
            } else {
                base.join(&image.path)
            };
            let (file_size, crc32) = file_crc(&path)?;
            let descriptor = ImageDescriptor {
                address: image.address.value()?,
                file_size,
                runtime_size: match &image.runtime_size {
                    Some(value) => value.value()?,
                    None => file_size,
                },
                crc32,
                flags: 0,
            };
            if !descriptor.validate() {
                return Err(format!(
                    "{} has an invalid guest address or runtime extent",
                    path.display()
                ));
            }
            if images
                .iter()
                .any(|other| overlaps(descriptor, other.descriptor))
            {
                return Err(format!("{} overlaps another bundle image", path.display()));
            }
            images.push(PreparedImage { path, descriptor });
        }
        let (flags, registers) = registers(&manifest)?;
        let descriptor = BundleDescriptor {
            entry: manifest.entry.value()?,
            image_count: images.len() as u32,
            flags,
            registers,
        };
        if !descriptor.validate()
            || !images
                .iter()
                .any(|image| image.descriptor.contains_entry(descriptor.entry))
        {
            return Err("bundle entry must be aligned and lie inside an uploaded image".into());
        }
        Ok(Self { descriptor, images })
    }

    pub fn single(
        path: &Path,
        address: u64,
        runtime_size: u64,
        entry: u64,
        flags: u32,
        registers: [u64; 8],
    ) -> Result<Self, String> {
        let (file_size, crc32) = file_crc(path)?;
        let image = ImageDescriptor {
            address,
            file_size,
            runtime_size,
            crc32,
            flags: 0,
        };
        let descriptor = BundleDescriptor {
            entry,
            image_count: 1,
            flags,
            registers,
        };
        if !image.validate() || !descriptor.validate() || !image.contains_entry(entry) {
            return Err("invalid image size, runtime extent, entry, or register policy".into());
        }
        Ok(Self {
            descriptor,
            images: vec![PreparedImage {
                path: path.to_path_buf(),
                descriptor: image,
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use switchvisor::loader::GUEST_RAM_BASE;

    #[test]
    fn directory_manifest_keeps_images_opaque_and_resolves_relative_paths() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "switchvisorctl-bundle-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("first.bin"), b"abcdefgh").unwrap();
        fs::write(directory.join("second.bin"), b"opaque").unwrap();
        fs::write(
            directory.join("bundle.json"),
            format!(
                r#"{{
  "version": 1,
  "entry": "{:#x}",
  "images": [
    {{"path": "first.bin", "address": "{:#x}", "runtime_size": 16}},
    {{"path": "second.bin", "address": "{:#x}"}}
  ]
}}"#,
                GUEST_RAM_BASE + 0x1000,
                GUEST_RAM_BASE + 0x1000,
                GUEST_RAM_BASE + 0x3000
            ),
        )
        .unwrap();

        let bundle = PreparedBundle::from_manifest(&directory).unwrap();
        assert_eq!(bundle.descriptor.image_count, 2);
        assert_eq!(bundle.images[0].descriptor.runtime_size, 16);
        assert_eq!(bundle.images[1].descriptor.file_size, 6);
        assert_eq!(bundle.images[0].path, directory.join("first.bin"));
        fs::remove_dir_all(directory).unwrap();
    }
}
