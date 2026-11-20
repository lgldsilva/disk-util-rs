//! Formatação de tamanhos e rótulos (port de `disk_usage.py`).

/// Formata bytes em unidades legíveis (`binary` = KiB/MiB..., `decimal` = KB/MB...).
pub fn format_size(bytes: u64, decimal: bool, precision: usize) -> String {
    let step: f64 = if decimal { 1000.0 } else { 1024.0 };
    let units: &[&str] = if decimal {
        &["B", "KB", "MB", "GB", "TB"]
    } else {
        &["B", "KiB", "MiB", "GiB", "TiB"]
    };
    let last = if decimal { "PB" } else { "PiB" };
    let mut v = bytes as f64;
    for unit in units {
        if v < step {
            return format!("{v:.precision$} {unit}");
        }
        v /= step;
    }
    format!("{v:.precision$} {last}")
}

/// Formata bytes em unidade curta (`12.3G`, `184K`, `42B`).
pub fn format_size_short(bytes: u64, decimal: bool) -> String {
    let step: f64 = if decimal { 1000.0 } else { 1024.0 };
    let suf = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    for s in suf {
        if v < step {
            if s == "B" {
                return format!("{bytes}B");
            }
            let out = format!("{v:.1}{s}");
            return if out.len() < 6 {
                out
            } else {
                format!("{}{s}", v as u64)
            };
        }
        v /= step;
    }
    format!("{}P", v as u64)
}

/// Trunca rótulo p/ caber no layout.
pub fn trunc_label(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let kept: String = s.chars().take(n.saturating_sub(3)).collect();
        format!("{kept}...")
    }
}

/// True p/ mounts de boot (`/boot`, `/boot/efi`), ocultos por padrão.
pub fn is_boot_mount(mountpoint: &str, boot_paths: &[String]) -> bool {
    boot_paths
        .iter()
        .any(|b| mountpoint == b || mountpoint.starts_with(&format!("{b}/")))
}

/// Cor/estilo lógico por percentual (usado pela TUI e pelo `--once`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Crit,
}

pub fn ram_level(pct: f64, warn: f64, crit: f64) -> Level {
    if pct >= crit {
        Level::Crit
    } else if pct >= warn {
        Level::Warn
    } else {
        Level::Ok
    }
}

pub fn disk_level(pct: f64, warn: f64, crit: f64) -> Level {
    ram_level(pct, warn, crit)
}

/// Barra `█/░` de 10 blocos p/ percentual 0..=100.
pub fn bar10(pct: f64) -> String {
    let pct = pct.clamp(0.0, 100.0);
    let filled = (pct / 10.0) as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(10 - filled))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_match_python_shape() {
        assert_eq!(format_size(0, false, 2), "0.00 B");
        assert_eq!(format_size(1024, false, 2), "1.00 KiB");
        assert_eq!(format_size_short(42, false), "42B");
        assert_eq!(trunc_label("abcdef", 24), "abcdef");
        assert_eq!(trunc_label("abcdefghij", 7), "abcd...");
    }

    #[test]
    fn boot_detection() {
        let boots = vec!["/boot".to_string()];
        assert!(is_boot_mount("/boot", &boots));
        assert!(is_boot_mount("/boot/efi", &boots));
        assert!(!is_boot_mount("/", &boots));
        assert!(!is_boot_mount("/bootloader", &boots));
    }

    #[test]
    fn levels_follow_thresholds() {
        assert_eq!(ram_level(90.0, 70.0, 85.0), Level::Crit);
        assert_eq!(ram_level(80.0, 70.0, 85.0), Level::Warn);
        assert_eq!(ram_level(10.0, 70.0, 85.0), Level::Ok);
        assert_eq!(bar10(100.0), "██████████");
        assert_eq!(bar10(0.0), "░░░░░░░░░░");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn short_never_empty(n in any::<u64>()) {
            prop_assert!(!format_size_short(n, false).is_empty());
            prop_assert!(!format_size_short(n, true).is_empty());
        }

        #[test]
        fn long_always_has_unit(n in any::<u64>(), dec in any::<bool>()) {
            let s = format_size(n, dec, 2);
            prop_assert!(s.contains(' '), "sem unidade: {s}");
        }

        #[test]
        fn trunc_never_exceeds(n in 0usize..64, s in "\\PC{0,40}") {
            let out = trunc_label(&s, n.max(3));
            prop_assert!(out.chars().count() <= n.max(3));
        }

        #[test]
        fn bar10_always_10_cells(p in -50.0f64..150.0) {
            prop_assert_eq!(bar10(p).chars().count(), 10);
        }
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    #[test]
    fn decimal_and_huge_units() {
        assert_eq!(format_size(1500, true, 2), "1.50 KB");
        assert_eq!(format_size_short(1000, true), "1.0K");
        assert_eq!(format_size_short(10_000, true), "10.0K");
        assert_eq!(format_size_short(999_900, true), "999K");
        assert_eq!(format_size_short(1_500_000, true), "1.5M");
        assert_eq!(format_size_short(1_500_000_000_000, true), "1.5T");
        assert!(format_size(u64::MAX, true, 2).ends_with("PB"));
        assert!(format_size(u64::MAX, false, 2).ends_with("PiB"));
        assert_eq!(format_size(999, true, 2), "999.00 B");
        assert_eq!(disk_level(95.0, 75.0, 90.0), Level::Crit);
        assert_eq!(disk_level(80.0, 75.0, 90.0), Level::Warn);
        assert_eq!(disk_level(10.0, 75.0, 90.0), Level::Ok);
    }
}
