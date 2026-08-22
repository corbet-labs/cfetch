//! Hardware detection and variant selection.
//!
//! The policy is fixed: **NPU > GPU > CPU**, always. An NPU is preferred even
//! where it is SLOWER than the GPU beside it — on one Intel Core Ultra laptop
//! a BERT encoder measured 3.1 ms on the integrated GPU against 6.9 ms on the
//! NPU — because latency is not what an NPU is for. It draws far less power,
//! and it is the one processor on the machine that nothing else is competing
//! for: the CPU runs the system and the GPU runs the display. Moving
//! inference off both is the point, and on a laptop it is the difference
//! between a search that costs battery and one that does not.
//!
//! Two independent facts decide whether an accelerator is usable, and both
//! must hold:
//!
//! 1. the DEVICE is present, and
//! 2. this build can actually drive it — a binary compiled without the
//!    vendor runtime cannot use a device just because it exists.
//!
//! Detection therefore reports evidence rather than a verdict: what was found
//! and what proved it. An operator who disagrees with the choice can see why
//! it was made, and a machine that reports nothing is telling us something
//! real rather than failing silently.

use std::path::Path;

/// Accelerator classes, ordered worst to best so `Ord` IS the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Cpu,
    Gpu,
    Npu,
}

/// A specific accelerator we know how to target.
///
/// Several variants are constructed only under one `target_os` — the Apple
/// ones on macOS, Qualcomm on Windows — so on any single platform the rest
/// look unused. They are the vocabulary of the variant matrix, not dead code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Device {
    IntelNpu,
    AmdNpu,
    QualcommNpu,
    AppleNeuralEngine,
    NvidiaGpu,
    AmdGpu,
    IntelGpu,
    AppleGpu,
    Cpu,
}

impl Device {
    pub fn class(self) -> Class {
        match self {
            Device::IntelNpu | Device::AmdNpu | Device::QualcommNpu | Device::AppleNeuralEngine => {
                Class::Npu
            }
            Device::NvidiaGpu | Device::AmdGpu | Device::IntelGpu | Device::AppleGpu => Class::Gpu,
            Device::Cpu => Class::Cpu,
        }
    }

    /// The silicon token in the variant name.
    pub fn token(self) -> &'static str {
        match self {
            Device::IntelNpu => "npu-intel",
            Device::AmdNpu => "npu-amd",
            Device::QualcommNpu => "npu-qc",
            Device::AppleNeuralEngine => "apple",
            Device::NvidiaGpu => "nvidia",
            Device::AmdGpu => "amd",
            Device::IntelGpu => "intel",
            Device::AppleGpu => "apple",
            Device::Cpu => "cpu",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Device::IntelNpu => "Intel NPU (AI Boost)",
            Device::AmdNpu => "AMD NPU (XDNA / Ryzen AI)",
            Device::QualcommNpu => "Qualcomm Hexagon NPU",
            Device::AppleNeuralEngine => "Apple Neural Engine",
            Device::NvidiaGpu => "NVIDIA GPU",
            Device::AmdGpu => "AMD GPU",
            Device::IntelGpu => "Intel GPU",
            Device::AppleGpu => "Apple GPU (Metal)",
            Device::Cpu => "CPU",
        }
    }
}

/// One detected accelerator, with what proved it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub device: Device,
    /// What proved the hardware exists — a path, a device id. Never a guess.
    pub evidence: String,
}

/// The x86-64 microarchitecture level this CPU satisfies, for the `-v3`/`-v4`
/// half of a variant name. Returns `None` off x86-64, where the concept does
/// not exist.
pub fn x86_64_level() -> Option<&'static str> {
    #[cfg(target_arch = "x86_64")]
    {
        // v4 adds AVX-512F/BW/CD/DL/VL over v3; v3 adds AVX2/BMI2/FMA/MOVBE
        // over v2. Anything older is just "legacy" — we do not enumerate the
        // pre-v2 world.
        if is_x86_feature_detected!("avx512f")
            && is_x86_feature_detected!("avx512bw")
            && is_x86_feature_detected!("avx512cd")
            && is_x86_feature_detected!("avx512vl")
        {
            return Some("v4");
        }
        if is_x86_feature_detected!("avx2")
            && is_x86_feature_detected!("bmi2")
            && is_x86_feature_detected!("fma")
        {
            return Some("v3");
        }
        Some("legacy")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// The OS token in the variant name.
pub fn os_token() -> &'static str {
    if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "windows") {
        "win"
    } else {
        "linux"
    }
}

/// Everything this machine offers, best first.
///
/// The CPU is always last and always present — it is the floor, not a
/// detection result.
pub fn detect() -> Vec<Found> {
    let mut found = detect_platform();
    // Highest class first; within a class, detection order is preserved so
    // the answer is stable rather than dependent on hashing.
    found.sort_by_key(|f| std::cmp::Reverse(f.device.class()));
    found.push(Found { device: Device::Cpu, evidence: "always available".into() });
    found
}

#[cfg(target_os = "linux")]
fn detect_platform() -> Vec<Found> {
    let mut out = Vec::new();
    out.extend(linux_accel_devices(Path::new("/dev"), Path::new("/sys/bus/pci/devices")));
    out.extend(linux_gpus(Path::new("/sys/class/drm")));
    out
}

#[cfg(target_os = "macos")]
fn detect_platform() -> Vec<Found> {
    // Every Apple-silicon Mac has both, and no Intel Mac has either. The
    // architecture IS the detection — there is no partial configuration to
    // probe for.
    if cfg!(target_arch = "aarch64") {
        vec![
            Found {
                device: Device::AppleNeuralEngine,
                evidence: "Apple silicon (ANE present on every arm64 Mac)".into(),
            },
            Found { device: Device::AppleGpu, evidence: "Apple silicon (Metal)".into() },
        ]
    } else {
        Vec::new()
    }
}

#[cfg(target_os = "windows")]
fn detect_platform() -> Vec<Found> {
    // Windows device enumeration needs SetupAPI/WMI, which is a real
    // dependency for a detection nicety. Reporting nothing found is honest;
    // claiming a CPU-only machine when an NPU is present would not be, so
    // this must be filled in before a Windows NPU variant ships.
    Vec::new()
}

/// NPUs on Linux present as `/dev/accel/accelN` (the `accel` subsystem) and
/// as PCI class 0x1200, "Processing accelerators". The vendor id on the
/// matching PCI function is what separates Intel from AMD.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_accel_devices(dev: &Path, pci: &Path) -> Vec<Found> {
    let mut out = Vec::new();
    let accel_dir = dev.join("accel");
    let has_accel = std::fs::read_dir(&accel_dir)
        .map(|rd| rd.flatten().any(|e| e.file_name().to_string_lossy().starts_with("accel")))
        .unwrap_or(false);
    if !has_accel {
        return out;
    }
    // Find which vendor owns a processing-accelerator function.
    let Ok(rd) = std::fs::read_dir(pci) else { return out };
    for entry in rd.flatten() {
        let p = entry.path();
        let class = read_trim(&p.join("class")).unwrap_or_default();
        if !class.starts_with("0x1200") {
            continue;
        }
        let vendor = read_trim(&p.join("vendor")).unwrap_or_default();
        let device = match vendor.as_str() {
            "0x8086" => Device::IntelNpu,
            "0x1022" | "0x1002" => Device::AmdNpu,
            _ => continue,
        };
        out.push(Found {
            device,
            evidence: format!(
                "{} (PCI class 0x1200, vendor {vendor}) with {}",
                p.file_name().unwrap_or_default().to_string_lossy(),
                accel_dir.display()
            ),
        });
    }
    out
}

/// GPUs by the DRM card's PCI vendor id.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_gpus(drm: &Path) -> Vec<Found> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(drm) else { return out };
    let mut cards: Vec<_> = rd
        .flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            n.starts_with("card") && !n.contains('-')
        })
        .collect();
    cards.sort_by_key(|e| e.file_name());
    for card in cards {
        let vendor = read_trim(&card.path().join("device/vendor")).unwrap_or_default();
        let device = match vendor.as_str() {
            "0x10de" => Device::NvidiaGpu,
            "0x1002" => Device::AmdGpu,
            "0x8086" => Device::IntelGpu,
            _ => continue,
        };
        if out.iter().any(|f: &Found| f.device == device) {
            continue; // one entry per vendor, not one per head
        }
        out.push(Found {
            device,
            evidence: format!(
                "{} (PCI vendor {vendor})",
                card.file_name().to_string_lossy()
            ),
        });
    }
    out
}

fn read_trim(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// The variant this machine should install, under the
/// `<os>-cfetch-<silicon>[-<level>]` scheme.
///
/// The best DETECTED device wins, whether or not this build can drive it —
/// the point of the name is to tell an installer what to fetch.
pub fn recommended_variant(found: &[Found]) -> String {
    let best = found.first().map(|f| f.device).unwrap_or(Device::Cpu);
    let mut name = format!("{}-cfetch-{}", os_token(), best.token());
    // The microarchitecture level qualifies CPU builds only: it says which
    // instructions the scalar code may use, which is meaningless once the
    // work happens on an accelerator.
    if best == Device::Cpu
        && let Some(level) = x86_64_level()
    {
        name.push('-');
        name.push_str(level);
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_is_the_ordering() {
        // NPU > GPU > CPU, as an Ord rather than as a comment someone can
        // forget to honor at a call site.
        assert!(Class::Npu > Class::Gpu);
        assert!(Class::Gpu > Class::Cpu);
        let mut v = [Class::Cpu, Class::Npu, Class::Gpu];
        v.sort();
        assert_eq!(v, [Class::Cpu, Class::Gpu, Class::Npu]);
    }

    #[test]
    fn detection_always_offers_the_cpu_floor_last() {
        let found = detect();
        assert!(!found.is_empty());
        assert_eq!(found.last().unwrap().device, Device::Cpu, "the CPU is the floor");
        // And the list is ordered best-first.
        for w in found.windows(2) {
            assert!(w[0].device.class() >= w[1].device.class(), "{found:?}");
        }
    }

    #[test]
    fn an_npu_outranks_a_gpu_in_the_recommended_variant() {
        let npu = Found { device: Device::IntelNpu, evidence: "test".into() };
        let gpu = Found { device: Device::AmdGpu, evidence: "test".into() };
        let cpu = Found { device: Device::Cpu, evidence: "test".into() };
        assert!(recommended_variant(&[npu, gpu.clone(), cpu.clone()]).contains("npu-intel"));
        assert!(recommended_variant(&[gpu, cpu.clone()]).contains("-amd"));
        // A CPU-only machine gets a microarchitecture level; an accelerated
        // one does not, because the level describes scalar code.
        let only_cpu = recommended_variant(&[cpu]);
        assert!(only_cpu.contains("-cpu"));
        #[cfg(target_arch = "x86_64")]
        assert!(only_cpu.ends_with("-v3") || only_cpu.ends_with("-v4") || only_cpu.ends_with("-legacy"));
    }

    #[test]
    fn the_variant_name_carries_the_platform() {
        let name = recommended_variant(&detect());
        assert!(name.starts_with(os_token()), "{name}");
        assert!(name.contains("-cfetch-"), "{name}");
    }

    // ---- Linux probing, against synthetic sysfs trees ----

    #[cfg(target_os = "linux")]
    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_intel_npu_is_recognised_by_accel_node_plus_pci_class() {
        let dir = tempfile::tempdir().unwrap();
        let (dev, pci) = (dir.path().join("dev"), dir.path().join("pci"));
        std::fs::create_dir_all(dev.join("accel")).unwrap();
        std::fs::write(dev.join("accel/accel0"), "").unwrap();
        write(&pci, "0000:00:0b.0/class", "0x120000\n");
        write(&pci, "0000:00:0b.0/vendor", "0x8086\n");

        let found = linux_accel_devices(&dev, &pci);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].device, Device::IntelNpu);
        assert!(found[0].evidence.contains("0x8086"), "{:?}", found[0].evidence);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pci_function_that_is_not_an_accelerator_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let (dev, pci) = (dir.path().join("dev"), dir.path().join("pci"));
        std::fs::create_dir_all(dev.join("accel")).unwrap();
        std::fs::write(dev.join("accel/accel0"), "").unwrap();
        // A display controller from the same vendor must not read as an NPU.
        write(&pci, "0000:00:02.0/class", "0x030000\n");
        write(&pci, "0000:00:02.0/vendor", "0x8086\n");
        assert!(linux_accel_devices(&dev, &pci).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_accel_node_means_no_npu_however_the_pci_bus_looks() {
        // The kernel driver has to be bound, not merely the silicon present:
        // an unbound NPU is one we cannot drive.
        let dir = tempfile::tempdir().unwrap();
        let (dev, pci) = (dir.path().join("dev"), dir.path().join("pci"));
        std::fs::create_dir_all(&dev).unwrap();
        write(&pci, "0000:00:0b.0/class", "0x120000\n");
        write(&pci, "0000:00:0b.0/vendor", "0x8086\n");
        assert!(linux_accel_devices(&dev, &pci).is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gpus_are_read_off_drm_cards_one_entry_per_vendor() {
        let dir = tempfile::tempdir().unwrap();
        let drm = dir.path();
        write(drm, "card0/device/vendor", "0x1002\n");
        write(drm, "card1/device/vendor", "0x1002\n"); // second head, same GPU vendor
        write(drm, "card2/device/vendor", "0x10de\n");
        // Connector directories carry a dash and are not cards.
        write(drm, "card0-DP-1/device/vendor", "0x1002\n");

        let found = linux_gpus(drm);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().any(|f| f.device == Device::AmdGpu));
        assert!(found.iter().any(|f| f.device == Device::NvidiaGpu));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_card_with_no_vendor_file_is_skipped_not_guessed_at() {
        // Seen in the wild: a DRM card whose vendor file is empty, and one
        // with no device/ directory at all. Reading either as a match would
        // invent a GPU that is not there.
        let dir = tempfile::tempdir().unwrap();
        let drm = dir.path();
        write(drm, "card0/device/vendor", "0x8086\n");
        write(drm, "card1/device/vendor", "\n");
        std::fs::create_dir_all(drm.join("card2")).unwrap(); // no device/ at all

        let found = linux_gpus(drm);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].device, Device::IntelGpu);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_machine_with_both_an_npu_and_an_igpu_prefers_the_npu() {
        // The common integrated shape: an NPU at PCI class 0x1200 and an
        // iGPU from the same vendor. The NPU must win even though the iGPU
        // is measurably FASTER — that is the policy, not an oversight.
        let dir = tempfile::tempdir().unwrap();
        let (dev, pci, drm) = (dir.path().join("dev"), dir.path().join("pci"), dir.path().join("drm"));
        std::fs::create_dir_all(dev.join("accel")).unwrap();
        std::fs::write(dev.join("accel/accel0"), "").unwrap();
        write(&pci, "0000:00:0b.0/class", "0x120000\n");
        write(&pci, "0000:00:0b.0/vendor", "0x8086\n");
        write(&drm, "card0/device/vendor", "0x8086\n");

        let mut found = linux_accel_devices(&dev, &pci);
        found.extend(linux_gpus(&drm));
        found.sort_by_key(|f| std::cmp::Reverse(f.device.class()));
        assert_eq!(found[0].device, Device::IntelNpu);
        assert!(recommended_variant(&found).ends_with("cfetch-npu-intel"), "{found:?}");
    }
}
