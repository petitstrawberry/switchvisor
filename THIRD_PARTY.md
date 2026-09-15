# Third-party code

Switchvisor's Tegra210 USB PHY, clock and XUDC implementation is adapted from
[Hekate](https://github.com/CTCaer/hekate/tree/e487de8fdd6ca9c3f608d1d18c097a86355912b9),
at revision `e487de8fdd6ca9c3f608d1d18c097a86355912b9`:

- `bdk/usb/xusbd.c`: Copyright (c) 2020-2025 CTCaer.
- `bdk/usb/usb_t210.h`: Copyright (c) 2019-2021 CTCaer.
- `bdk/soc/clock.c`: Copyright (c) 2018 naehrwert; 2018-2026 CTCaer.
- `bdk/soc/clock.h`: Copyright (c) 2018 naehrwert; 2018-2025 CTCaer.
- `bdk/soc/t210.h`: Copyright (c) 2018 naehrwert; 2018-2023 CTCaer.

These sources are licensed under the GNU General Public License version 2 only.
The Rust adaptation is in `crates/switchvisor/src/drivers/usb/tegra210.rs`.
The complete license text is in [LICENSE](LICENSE).

The macOS development shell builds the standalone
[NXBoot](https://github.com/mologie/nxboot) command at revision
`5ba7bce91840e41fb2bdfeba47482c95ccdea7e9`. NXBoot is licensed under the GNU
General Public License version 3 and is used as a separate RCM payload launcher;
it is not linked into Switchvisor.
