use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use switchvisor_core::memory::{
    AddressRange, BootProfile, GuestRegion, GuestRegionKind, NamedRegion,
};

#[derive(Clone, Copy)]
struct Address(u64);

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Input {
            Number(u64),
            Text(String),
        }
        match Input::deserialize(deserializer)? {
            Input::Number(n) => Ok(Self(n)),
            Input::Text(s) => {
                let number = if let Some(hex) = s.strip_prefix("0x") {
                    u64::from_str_radix(hex, 16)
                } else {
                    s.parse()
                };
                number.map(Self).map_err(serde::de::Error::custom)
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RangeSpec {
    base: Option<Address>,
    size: Option<Address>,
}

fn range(
    base: Option<Address>,
    size: Option<Address>,
    label: &str,
) -> Result<AddressRange, String> {
    let base = base
        .ok_or_else(|| format!("{label}: base is unresolved"))?
        .0;
    let size = size
        .ok_or_else(|| {
            format!("{label}: size is unresolved (use runtime extent, not just file size)")
        })?
        .0;
    AddressRange::new(base, size).map_err(|e| format!("{label}: {e}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedSpec {
    name: String,
    base: Option<Address>,
    size: Option<Address>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    ScarletRuntime,
    OsDtb,
    Initramfs,
    Uimage,
    PlatformDtimg,
    UbootImage,
    UbootInitialRam,
    UbootRelocation,
    BootScript,
}

impl Kind {
    fn core(self) -> GuestRegionKind {
        match self {
            Self::ScarletRuntime => GuestRegionKind::ScarletRuntime,
            Self::OsDtb => GuestRegionKind::OsDtb,
            Self::Initramfs => GuestRegionKind::Initramfs,
            Self::Uimage => GuestRegionKind::Uimage,
            Self::PlatformDtimg => GuestRegionKind::PlatformDtimg,
            Self::UbootImage => GuestRegionKind::UbootImage,
            Self::UbootInitialRam => GuestRegionKind::UbootInitialRam,
            Self::UbootRelocation => GuestRegionKind::UbootRelocation,
            Self::BootScript => GuestRegionKind::BootScript,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GuestSpec {
    kind: Kind,
    name: String,
    base: Option<Address>,
    size: Option<Address>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileSpec {
    schema_version: u32,
    name: String,
    soc: String,
    sku: u32,
    memory_map_verified: bool,
    uboot_layout_verified: bool,
    hv_region: RangeSpec,
    usb_dma_region: RangeSpec,
    usable_banks: Vec<RangeSpec>,
    protected_regions: Vec<NamedSpec>,
    guest_regions: Vec<GuestSpec>,
}

pub fn validate(bytes: &[u8]) -> Result<Value, String> {
    let spec: ProfileSpec =
        serde_json::from_slice(bytes).map_err(|e| format!("profile JSON: {e}"))?;
    if spec.schema_version != 1 {
        return Err("unsupported profile schema_version".into());
    }
    if spec.soc != "tegra210-erista" {
        return Err("only tegra210-erista is supported".into());
    }
    if spec.name.trim().is_empty() {
        return Err("profile name is empty".into());
    }
    let hv = range(spec.hv_region.base, spec.hv_region.size, "hv_region")?;
    let dma = range(
        spec.usb_dma_region.base,
        spec.usb_dma_region.size,
        "usb_dma_region",
    )?;
    let banks: Vec<_> = spec
        .usable_banks
        .iter()
        .enumerate()
        .map(|(i, r)| range(r.base, r.size, &format!("usable_banks[{i}]")))
        .collect::<Result<_, _>>()?;
    let protected: Vec<_> = spec
        .protected_regions
        .iter()
        .map(|r| {
            Ok(NamedRegion {
                name: r.name.as_str(),
                range: range(r.base, r.size, &r.name)?,
            })
        })
        .collect::<Result<_, String>>()?;
    let guest: Vec<_> = spec
        .guest_regions
        .iter()
        .map(|r| {
            Ok(GuestRegion {
                kind: r.kind.core(),
                region: NamedRegion {
                    name: r.name.as_str(),
                    range: range(r.base, r.size, &r.name)?,
                },
            })
        })
        .collect::<Result<_, String>>()?;
    let profile = BootProfile {
        sku: spec.sku,
        memory_map_verified: spec.memory_map_verified,
        uboot_layout_verified: spec.uboot_layout_verified,
        hv_region: Some(hv),
        usb_dma_region: Some(dma),
        usable_banks: &banks,
        protected_regions: &protected,
        guest_regions: &guest,
    };
    profile
        .validate()
        .map_err(|e| format!("profile validation failed: {e}"))?;
    Ok(json!({
        "name": spec.name, "soc": spec.soc, "sku": spec.sku,
        "validation": "declared physical geometry", "hardware_validated": false,
        "hv_base": format!("{:#x}", hv.start()), "hv_size_bytes": hv.size(),
        "usb_dma_base": format!("{:#x}", dma.start()), "usb_dma_size_bytes": dma.size(),
        "usable_bank_count": banks.len(), "protected_region_count": protected.len(), "guest_region_count": guest.len()
    }))
}
