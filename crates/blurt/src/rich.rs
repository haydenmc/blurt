//! "Rich visuals": drawing with the GPU (wgpu) instead of the CPU
//! (tiny-skia), for effects tiny-skia can't do cheaply, like the room glow.
//!
//! The renderer is picked once, at startup, so changing the setting takes a
//! restart. Auto turns it on only for machines with room to spare: 32 GB
//! of memory and a real GPU (a software one, like llvmpipe, would cost more
//! memory and CPU than it's worth).

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RichVisuals {
    #[default]
    Auto,
    On,
    Off,
}

impl RichVisuals {
    /// Whether to ask for the GPU renderer at startup.
    pub fn wanted(self) -> bool {
        match self {
            RichVisuals::On => true,
            RichVisuals::Off => false,
            RichVisuals::Auto => capable(),
        }
    }
}

/// Whether this machine is one auto turns rich visuals on for.
pub fn capable() -> bool {
    use std::sync::OnceLock;
    static CAPABLE: OnceLock<bool> = OnceLock::new();
    *CAPABLE.get_or_init(|| {
        let memory = memory_bytes();
        let gpu = hardware_gpu();
        log::info!(
            "Rich visuals auto check: {} GB memory, hardware GPU: {gpu}",
            memory.unwrap_or(0) / 1_000_000_000
        );
        // "32 GB" machines report a little under 32 GiB.
        memory.is_some_and(|m| m >= 30 * (1 << 30)) && gpu
    })
}

/// Total memory, from `/proc/meminfo`.
fn memory_bytes() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_mem_total(&meminfo)
}

fn parse_mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Whether there's a GPU render node driven by real hardware. Checked from
/// sysfs, without loading any GPU driver into this process.
fn hardware_gpu() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("renderD") {
            return false;
        }
        let driver = std::fs::read_link(entry.path().join("device/driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        driver.is_some_and(|d| is_hardware_driver(&d))
    })
}

/// Virtual and software DRM drivers aren't worth rendering with.
fn is_hardware_driver(driver: &str) -> bool {
    !matches!(
        driver,
        "vgem" | "vkms" | "simpledrm" | "virtio_gpu" | "vmwgfx" | "qxl"
    )
}

/// Asks iced for the renderer: the GPU one when `wanted`, else the CPU one.
/// Leaves `ICED_BACKEND` alone if the user set it themselves. Call before
/// anything else runs, while the process is single-threaded.
pub fn choose_renderer(wanted: bool) {
    if std::env::var_os("ICED_BACKEND").is_some() {
        return;
    }
    if !wanted || !cfg!(feature = "gpu") {
        // SAFETY: called first thing in `main`, before any other threads
        // exist that could read the environment.
        unsafe { std::env::set_var("ICED_BACKEND", "tiny-skia") };
    }
}

/// What actually draws, once known: the GPU can fail to start, and then
/// iced falls back to the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    Unknown,
    Gpu,
    Cpu,
}

static RENDERER: AtomicU8 = AtomicU8::new(0);

pub fn renderer() -> Renderer {
    match RENDERER.load(Ordering::Relaxed) {
        1 => Renderer::Gpu,
        2 => Renderer::Cpu,
        _ => Renderer::Unknown,
    }
}

/// Records what drew the last frame (see `ui::glow`).
#[cfg_attr(not(feature = "gpu"), allow(dead_code))]
pub fn set_renderer(renderer: Renderer) {
    let value = match renderer {
        Renderer::Unknown => 0,
        Renderer::Gpu => 1,
        Renderer::Cpu => 2,
    };
    RENDERER.store(value, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_mem_total() {
        let meminfo = "MemTotal:       32718376 kB\nMemFree:  1 kB\n";
        assert_eq!(parse_mem_total(meminfo), Some(32718376 * 1024));
        assert_eq!(parse_mem_total("nothing"), None);
    }

    #[test]
    fn software_drivers_dont_count() {
        assert!(is_hardware_driver("amdgpu"));
        assert!(is_hardware_driver("i915"));
        assert!(is_hardware_driver("nvidia"));
        assert!(!is_hardware_driver("vgem"));
        assert!(!is_hardware_driver("simpledrm"));
    }
}
