//! Host RAM and GPU VRAM for the panel's resource line, read from Linux's `/proc` and `sysfs`.
//! A file that is missing or unreadable gives `None`, which the panel shows as "n/a".

use serde_json::{Value, json};
use std::path::Path;

/// Used and total bytes of one memory pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub used: u64,
    pub total: u64,
}

/// Reads `MemTotal` and `MemAvailable` from `/proc/meminfo` text. Used is total minus available,
/// which is what Linux itself counts as memory in use, not counting caches it can reclaim.
pub fn parse_meminfo(text: &str) -> Option<Usage> {
    let field = |name: &str| {
        let line = text
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))?;
        let mut parts = line.split_whitespace();
        let kib: u64 = parts.next()?.parse().ok()?;
        (parts.next()? == "kB").then_some(kib.checked_mul(1024)?)
    };
    let total = field("MemTotal")?;
    let available = field("MemAvailable")?;
    Some(Usage {
        used: total.saturating_sub(available),
        total,
    })
}

/// Reads the amdgpu `mem_info_vram_used` and `mem_info_vram_total` files, each one byte count.
pub fn parse_vram(used: &str, total: &str) -> Option<Usage> {
    Some(Usage {
        used: used.trim().parse().ok()?,
        total: total.trim().parse().ok()?,
    })
}

/// The first `card*` under `drm` that reports VRAM use, for hosts with one AMD GPU.
// shortcut: first card wins, so a host with several GPUs shows only one; upgrade if that happens.
pub fn vram_in(drm: &Path) -> Option<Usage> {
    let mut devices: Vec<_> = std::fs::read_dir(drm)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("card"))
        .map(|entry| entry.path().join("device"))
        .collect();
    devices.sort();
    devices.iter().find_map(|device| {
        let used = std::fs::read_to_string(device.join("mem_info_vram_used")).ok()?;
        let total = std::fs::read_to_string(device.join("mem_info_vram_total")).ok()?;
        parse_vram(&used, &total)
    })
}

/// The resource snapshot the agent sends the panel: `{"ram": {used, total} | null, "vram": ...}`.
pub fn snapshot() -> Value {
    let ram = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| parse_meminfo(&text));
    let vram = vram_in(Path::new("/sys/class/drm"));
    json!({"ram": usage_json(ram), "vram": usage_json(vram)})
}

fn usage_json(usage: Option<Usage>) -> Value {
    usage.map_or(Value::Null, |u| json!({"used": u.used, "total": u.total}))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "MemTotal:       65011712 kB\nMemFree:        1024000 kB\n\
        MemAvailable:   50331648 kB\nBuffers:          12345 kB\n";

    #[test]
    fn ram_used_is_total_minus_available_in_bytes() {
        assert_eq!(
            parse_meminfo(MEMINFO),
            Some(Usage {
                used: (65_011_712 - 50_331_648) * 1024,
                total: 65_011_712 * 1024,
            })
        );
    }

    #[test]
    fn meminfo_without_both_fields_or_with_another_unit_gives_none() {
        assert_eq!(parse_meminfo("MemTotal: 100 kB\n"), None);
        assert_eq!(
            parse_meminfo("MemTotal: 100 MB\nMemAvailable: 50 MB\n"),
            None
        );
        assert_eq!(
            parse_meminfo("MemTotal: lots kB\nMemAvailable: 1 kB\n"),
            None
        );
        assert_eq!(parse_meminfo(""), None);
    }

    #[test]
    fn vram_files_hold_a_bare_byte_count_with_a_trailing_newline() {
        assert_eq!(
            parse_vram("4294967296\n", "17163091968\n"),
            Some(Usage {
                used: 4_294_967_296,
                total: 17_163_091_968,
            })
        );
        assert_eq!(parse_vram("", "17163091968"), None);
        assert_eq!(parse_vram("12 MiB", "17163091968"), None);
    }

    #[test]
    fn a_missing_drm_tree_gives_no_vram() {
        let dir = std::env::temp_dir().join("praxis-no-such-drm-tree");
        assert_eq!(vram_in(&dir), None);
    }

    #[test]
    fn a_snapshot_marks_what_it_cannot_read_as_null() {
        let snap = snapshot();
        assert!(snap["ram"].is_null() || snap["ram"]["total"].is_u64());
        assert!(snap["vram"].is_null() || snap["vram"]["total"].is_u64());
    }
}
