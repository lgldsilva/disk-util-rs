//! Relatório `--once`: saída texto schema-estável (sem TUI).

use std::collections::HashMap;

use crate::config::AppConfig;
use crate::format::{format_size, format_size_short, is_boot_mount};
use crate::scan::{ItemStatus, ScanItem};
use crate::system::{PartitionView, SystemSnapshot, VolumeUsage};

pub struct OnceOpts {
    pub show_boot: bool,
    pub cloud_size: bool,
    pub only_volumes: bool,
    pub only_folders: bool,
}

/// Linha de mount remoto do relatório (fake via cache/nuvem ou cota do pai).
fn push_remote_line(
    out: &mut String,
    p: &PartitionView,
    u: &VolumeUsage,
    cfg: &AppConfig,
    snap: &SystemSnapshot,
    remotes_parents: &HashMap<String, String>,
    opts: &OnceOpts,
) {
    if u.fake {
        let used_s = if opts.cloud_size {
            let rn = crate::rclone::remote_name_from_device(&p.device);
            match crate::system::cache_apparent_for(rn, snap) {
                Some(a) if a > 0 && a < cfg.rclone.fake_size_threshold => {
                    format_size_short(a, true)
                }
                _ => "—".to_string(),
            }
        } else {
            "—".to_string()
        };
        out.push_str(&format!(
            "{} [rclone] total=— used={used_s} free=— cloud\n",
            p.mountpoint
        ));
        return;
    }
    let pct = if u.total > 0 {
        u.used as f64 / u.total as f64 * 100.0
    } else {
        0.0
    };
    let shared = remotes_parents
        .get(&p.mountpoint)
        .map(|s| format!(" parent={s}"))
        .unwrap_or_default();
    out.push_str(&format!(
        "{} total={} used={} free={} pct={:.1}%{}\n",
        p.mountpoint,
        format_size_short(u.total, p.remote),
        format_size_short(u.used, p.remote),
        format_size_short(u.free, p.remote),
        pct,
        shared
    ));
}

/// Monta o relatório completo como string (testável; `main` imprime).
pub fn render_report(
    dir: &str,
    cfg: &AppConfig,
    snap: &SystemSnapshot,
    items: &[&ScanItem],
    remotes_parents: &HashMap<String, String>,
    opts: &OnceOpts,
) -> String {
    let dec = cfg.display.byte_format == "decimal";
    let prec = cfg.display.precision;
    let mut out = String::new();
    out.push_str(&format!("disk-util-rs | Diretório: {dir}\n"));
    if !opts.only_folders {
        out.push_str("== RAM ==\n");
        if snap.ram_total > 0 {
            out.push_str(&format!(
                "RAM total={} used={} avail={} pct={:.1}%\n",
                format_size(snap.ram_total, dec, prec),
                format_size(snap.ram_used, dec, prec),
                format_size(snap.ram_avail, dec, prec),
                snap.ram_pct
            ));
        }
        if snap.swap_total > 0 {
            out.push_str(&format!(
                "SWAP total={} used={} free={} pct={:.1}%\n",
                format_size(snap.swap_total, dec, prec),
                format_size(snap.swap_used, dec, prec),
                format_size(snap.swap_free, dec, prec),
                snap.swap_pct
            ));
        } else {
            out.push_str("SWAP Desativada\n");
        }

        out.push_str("== Volumes ==\n");
        let mut parts: Vec<_> = snap.partitions.iter().collect();
        parts.sort_by(|a, b| a.mountpoint.cmp(&b.mountpoint));
        let boot_visible = |mp: &str| opts.show_boot || !is_boot_mount(mp, &cfg.boot.paths);
        if snap.disk_groups.is_empty() {
            // Legado: uma linha por mountpoint (group_by="device").
            for p in parts {
                if !boot_visible(&p.mountpoint) {
                    continue;
                }
                let Some(u) = snap.usages.get(&p.mountpoint) else {
                    continue;
                };
                if p.remote {
                    push_remote_line(&mut out, p, u, cfg, snap, remotes_parents, opts);
                } else {
                    let pct = if u.total > 0 {
                        u.used as f64 / u.total as f64 * 100.0
                    } else {
                        0.0
                    };
                    out.push_str(&format!(
                        "{} total={} used={} free={} pct={:.1}%\n",
                        p.mountpoint,
                        format_size_short(u.total, p.remote),
                        format_size_short(u.used, p.remote),
                        format_size_short(u.free, p.remote),
                        pct
                    ));
                }
            }
        } else {
            // Consolidado: uma linha por disco físico/pool + remotos por mount.
            for g in &snap.disk_groups {
                let mps: Vec<&str> = g
                    .mountpoints
                    .iter()
                    .map(String::as_str)
                    .filter(|mp| boot_visible(mp))
                    .collect();
                if mps.is_empty() {
                    continue;
                }
                let pct = if g.total > 0 {
                    g.used as f64 / g.total as f64 * 100.0
                } else {
                    0.0
                };
                out.push_str(&format!(
                    "{} ({}) total={} used={} free={} pct={:.1}%\n",
                    g.label,
                    mps.join(","),
                    format_size_short(g.total, dec),
                    format_size_short(g.used, dec),
                    format_size_short(g.free, dec),
                    pct
                ));
            }
            for p in parts {
                if !p.remote || !boot_visible(&p.mountpoint) {
                    continue;
                }
                let Some(u) = snap.usages.get(&p.mountpoint) else {
                    continue;
                };
                push_remote_line(&mut out, p, u, cfg, snap, remotes_parents, opts);
            }
        }
        out.push_str(&format!(
            "FÍSICO total={} used={} free={}\n",
            format_size_short(snap.total_physical, dec),
            format_size_short(snap.used_physical, dec),
            format_size_short(snap.free_physical, dec)
        ));
        out.push_str(&format!(
            "NUVEM total={} used={} free={}\n",
            format_size_short(snap.total_cloud, dec),
            format_size_short(snap.used_cloud, dec),
            format_size_short(snap.free_cloud, dec)
        ));
        out.push_str(&format!(
            "VIRTUAL total={} used={} free={}\n",
            format_size_short(snap.total_virtual, dec),
            format_size_short(snap.used_virtual, dec),
            format_size_short(snap.free_virtual, dec)
        ));
        if snap.rclone_cache_total > 0 {
            out.push_str(&format!(
                "CACHE rclone total={}\n",
                format_size_short(snap.rclone_cache_total, dec)
            ));
        }

        if !snap.remote_hosts.is_empty() || !snap.ssh_errors.is_empty() {
            out.push_str("== SSH ==\n");
            // Falhas primeiro: motivo legível por host fora do ar.
            let mut failed: Vec<_> = snap.ssh_errors.iter().collect();
            failed.sort_by(|a, b| a.0.cmp(b.0));
            for (host, err) in failed {
                out.push_str(&format!("{host} FALHOU: {err}\n"));
            }
            let mut hosts: Vec<_> = snap.remote_hosts.iter().collect();
            hosts.sort_by(|a, b| a.0.cmp(b.0));
            for (host, parts) in hosts {
                for p in parts {
                    if !opts.show_boot && is_boot_mount(&p.mountpoint, &cfg.boot.paths) {
                        continue;
                    }
                    out.push_str(&format!(
                        "{host} {} total={} used={} free={} pct={:.1}%\n",
                        p.mountpoint,
                        format_size_short(p.size, p.is_remote),
                        format_size_short(p.used, p.is_remote),
                        format_size_short(p.free, p.is_remote),
                        p.used_pct
                    ));
                }
            }
        }
    } // fim !only_folders (RAM+Volumes+SSH)

    if !opts.only_volumes {
        out.push_str("== Pastas ==\n");
        for item in items {
            let status = match item.status {
                ItemStatus::Ok => "ok",
                ItemStatus::Calculating => "calculando",
                ItemStatus::Timeout => "timeout",
                ItemStatus::Error => "erro",
            };
            out.push_str(&format!(
                "{} type={} physical={} logical={} status={status}\n",
                item.name,
                if item.is_dir { "dir" } else { "file" },
                format_size(item.size_physical, dec, prec),
                format_size(item.size_logical, dec, prec),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::ScanItem;
    use std::path::PathBuf;

    fn test_cfg() -> AppConfig {
        AppConfig::default()
    }

    #[test]
    fn report_has_stable_schema_sections() {
        let cfg = test_cfg();
        let snap = SystemSnapshot {
            ram_total: 100,
            ram_used: 40,
            ram_avail: 60,
            ram_pct: 40.0,
            ..Default::default()
        };
        let items = [ScanItem {
            name: "a".to_string(),
            is_dir: true,
            path: PathBuf::from("/tmp/a"),
            size_logical: 10,
            size_physical: 20,
            status: ItemStatus::Ok,
        }];
        let refs: Vec<&ScanItem> = items.iter().collect();
        let out = render_report(
            "/tmp",
            &cfg,
            &snap,
            &refs,
            &HashMap::new(),
            &OnceOpts {
                show_boot: false,
                cloud_size: false,
                only_volumes: false,
                only_folders: false,
            },
        );
        for section in [
            "disk-util-rs | Diretório:",
            "== RAM ==",
            "== Volumes ==",
            "FÍSICO",
            "NUVEM",
            "VIRTUAL",
            "== Pastas ==",
        ] {
            assert!(out.contains(section), "faltando {section}:\n{out}");
        }
        assert!(out.contains("type=dir"));
        assert!(out.contains("status=ok"));
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::ssh::RemotePartition;
    use crate::system::{CacheDetail, DiskGroupView, PartitionView, SystemSnapshot, VolumeUsage};

    fn cfg() -> AppConfig {
        AppConfig::default()
    }

    fn snap_full() -> SystemSnapshot {
        let mut usages = HashMap::new();
        usages.insert(
            "/".to_string(),
            VolumeUsage {
                total: 100,
                used: 90,
                free: 10,
                fake: false,
            },
        );
        usages.insert(
            "/boot".to_string(),
            VolumeUsage {
                total: 10,
                used: 1,
                free: 9,
                fake: false,
            },
        );
        usages.insert(
            "/mnt/g".to_string(),
            VolumeUsage {
                total: 0,
                used: 7,
                free: 0,
                fake: true,
            },
        );
        let mut mount_parents = HashMap::new();
        mount_parents.insert("/mnt/g".to_string(), "gdrive".to_string());
        let mut remote_hosts = HashMap::new();
        remote_hosts.insert(
            "h".to_string(),
            vec![RemotePartition {
                device: "/dev/sda1".to_string(),
                fstype: String::new(),
                size: 100,
                used: 10,
                free: 90,
                used_pct: 10.0,
                mountpoint: "/boot".to_string(),
                is_remote: false,
            }],
        );
        SystemSnapshot {
            ram_total: 8,
            ram_used: 4,
            ram_avail: 4,
            ram_pct: 50.0,
            swap_total: 0,
            swap_used: 0,
            swap_free: 0,
            swap_pct: 0.0,
            partitions: vec![
                PartitionView {
                    device: "/dev/s".to_string(),
                    mountpoint: "/".to_string(),
                    fstype: "ext4".to_string(),
                    remote: false,
                },
                PartitionView {
                    device: "/dev/s".to_string(),
                    mountpoint: "/boot".to_string(),
                    fstype: "vfat".to_string(),
                    remote: false,
                },
                PartitionView {
                    device: "gdrive:".to_string(),
                    mountpoint: "/mnt/g".to_string(),
                    fstype: "fuse.rclone".to_string(),
                    remote: true,
                },
            ],
            usages,
            disk_groups: Vec::new(),
            mount_parents,
            remotes_parent_count: HashMap::new(),
            parent_totals: HashMap::new(),
            parent_used_sum: HashMap::new(),
            total_physical: 100,
            used_physical: 90,
            free_physical: 10,
            total_cloud: 0,
            used_cloud: 0,
            free_cloud: 0,
            total_virtual: 100,
            used_virtual: 90,
            free_virtual: 10,
            rclone_cache_total: 42,
            cache_details: vec![CacheDetail {
                name: "gdrive".to_string(),
                allocated: 42,
                apparent: 43,
            }],
            remote_hosts,
            ssh_errors: HashMap::new(),
        }
    }

    fn items_all_status() -> Vec<ScanItem> {
        use std::path::PathBuf;
        vec![
            ScanItem {
                name: "ok".to_string(),
                is_dir: true,
                path: PathBuf::from("ok"),
                size_logical: 1,
                size_physical: 2,
                status: ItemStatus::Ok,
            },
            ScanItem {
                name: "w".to_string(),
                is_dir: true,
                path: PathBuf::from("w"),
                size_logical: 0,
                size_physical: 0,
                status: ItemStatus::Calculating,
            },
            ScanItem {
                name: "t".to_string(),
                is_dir: true,
                path: PathBuf::from("t"),
                size_logical: 0,
                size_physical: 0,
                status: ItemStatus::Timeout,
            },
            ScanItem {
                name: "e".to_string(),
                is_dir: false,
                path: PathBuf::from("e"),
                size_logical: 0,
                size_physical: 0,
                status: ItemStatus::Error,
            },
        ]
    }

    #[test]
    fn report_groups_volumes_by_disk_when_present() {
        let cfg = cfg();
        let mut snap = snap_full();
        snap.disk_groups = vec![DiskGroupView {
            label: "sda".into(),
            mountpoints: vec!["/".into(), "/boot".into()],
            total: 110,
            used: 91,
            free: 19,
        }];
        let out = render_report(
            "/x",
            &cfg,
            &snap,
            &[],
            &snap.mount_parents,
            &OnceOpts {
                show_boot: true,
                cloud_size: false,
                only_volumes: false,
                only_folders: false,
            },
        );
        assert!(out.contains("sda (/,/boot) total=110B"), "{out}");
        // Linhas locais por mountpoint (legado) somem quando há grupos.
        assert!(!out.contains("/ total=100B"), "{out}");
        // Remote continua por mountpoint mesmo com grupos.
        assert!(out.contains("/mnt/g [rclone] total=— used=—"), "{out}");
    }

    #[test]
    fn report_lists_failed_ssh_hosts() {
        let cfg = cfg();
        let mut snap = snap_full();
        snap.remote_hosts.clear();
        snap.ssh_errors
            .insert("down".into(), "timeout: sem resposta após 4s".into());
        let out = render_report(
            "/x",
            &cfg,
            &snap,
            &[],
            &snap.mount_parents,
            &OnceOpts {
                show_boot: false,
                cloud_size: false,
                only_volumes: false,
                only_folders: false,
            },
        );
        assert!(out.contains("== SSH =="), "{out}");
        assert!(
            out.contains("down FALHOU: timeout: sem resposta após 4s"),
            "{out}"
        );
        // Sem hosts ok e sem falhas: seção some.
        let mut snap = snap_full();
        snap.remote_hosts.clear();
        let out = render_report(
            "/x",
            &cfg,
            &snap,
            &[],
            &snap.mount_parents,
            &OnceOpts {
                show_boot: false,
                cloud_size: false,
                only_volumes: false,
                only_folders: false,
            },
        );
        assert!(!out.contains("== SSH =="), "{out}");
    }

    #[test]
    fn full_report_covers_all_branches() {
        let cfg = cfg();
        let snap = snap_full();
        let items = items_all_status();
        let refs: Vec<&ScanItem> = items.iter().collect();
        let out = render_report(
            "/x",
            &cfg,
            &snap,
            &refs,
            &snap.mount_parents,
            &OnceOpts {
                show_boot: true,
                cloud_size: true,
                only_volumes: false,
                only_folders: false,
            },
        );
        assert!(out.contains("RAM total="));
        assert!(out.contains("h /boot total=100B"));
        for marker in [
            "== RAM ==",
            "SWAP Desativada",
            "== Volumes ==",
            "/boot",
            "cloud",
            "== SSH ==",
            "== Pastas ==",
            "type=file",
            "status=timeout",
            "status=erro",
            "status=calculando",
            "CACHE rclone",
        ] {
            assert!(out.contains(marker), "faltando {marker}:\n{out}");
        }
        assert!(out.contains("/mnt/g [rclone] total=— used=43B"));
        assert!(out.contains("h /boot total=100B"));
        // Sem boot + sem cloud-size: ramos alternativos.
        let out2 = render_report(
            "/x",
            &cfg,
            &snap,
            &refs,
            &snap.mount_parents,
            &OnceOpts {
                show_boot: false,
                cloud_size: false,
                only_volumes: false,
                only_folders: false,
            },
        );
        assert!(!out2.contains("/boot total="));
        assert!(out2.contains("used=—"));
        // Flags only-volumes / only-folders: paridade Python.
        let out_vol = render_report(
            "/x",
            &cfg,
            &snap,
            &refs,
            &snap.mount_parents,
            &OnceOpts {
                show_boot: false,
                cloud_size: false,
                only_volumes: true,
                only_folders: false,
            },
        );
        assert!(out_vol.contains("== RAM =="));
        assert!(!out_vol.contains("== Pastas =="));
        let out_fol = render_report(
            "/x",
            &cfg,
            &snap,
            &refs,
            &snap.mount_parents,
            &OnceOpts {
                show_boot: false,
                cloud_size: false,
                only_volumes: false,
                only_folders: true,
            },
        );
        assert!(!out_fol.contains("== RAM =="));
        assert!(!out_fol.contains("== Volumes =="));
        assert!(out_fol.contains("== Pastas =="));
    }

    #[test]
    fn empty_and_zero_volume_edges_render_without_cache_or_ssh() {
        let cfg = cfg();
        let mut snap = snap_full();
        snap.ram_total = 0;
        snap.swap_total = 100;
        snap.swap_used = 25;
        snap.swap_free = 75;
        snap.cache_details.clear();
        snap.rclone_cache_total = 0;
        snap.usages.remove("/boot");
        snap.remote_hosts.clear();
        snap.partitions.push(PartitionView {
            device: "/dev/zero".into(),
            mountpoint: "/mnt/zero".into(),
            fstype: "ext4".into(),
            remote: false,
        });
        snap.usages.insert(
            "/mnt/zero".into(),
            VolumeUsage {
                total: 0,
                used: 0,
                free: 0,
                fake: false,
            },
        );
        snap.partitions.push(PartitionView {
            device: "/dev/missing".into(),
            mountpoint: "/mnt/missing".into(),
            fstype: "ext4".into(),
            remote: false,
        });
        snap.partitions.push(PartitionView {
            device: "server:share".into(),
            mountpoint: "/mnt/real".into(),
            fstype: "nfs".into(),
            remote: true,
        });
        snap.usages.insert(
            "/mnt/real".into(),
            VolumeUsage {
                total: 100,
                used: 40,
                free: 60,
                fake: false,
            },
        );
        snap.mount_parents
            .insert("/mnt/real".into(), "server:share".into());

        let out = render_report(
            "/x",
            &cfg,
            &snap,
            &[],
            &snap.mount_parents,
            &OnceOpts {
                show_boot: true,
                cloud_size: true,
                only_volumes: false,
                only_folders: false,
            },
        );
        assert!(!out.contains("RAM total="));
        assert!(out.contains("SWAP total="));
        assert!(out.contains("/mnt/g [rclone] total=— used=—"));
        assert!(out.contains("/mnt/zero total=0B used=0B free=0B pct=0.0%"));
        assert!(
            out.contains("/mnt/real total=100B used=40B free=60B pct=40.0% parent=server:share")
        );
        assert!(!out.contains("/mnt/missing total="));
        assert!(!out.contains("CACHE rclone"));
        assert!(!out.contains("== SSH =="));
    }

    #[test]
    fn report_respects_decimal_format_and_cloud_size_boundaries() {
        let mut cfg = cfg();
        cfg.display.byte_format = "decimal".into();
        let mut snap = snap_full();
        snap.ram_total = 1500;
        snap.ram_used = 500;
        snap.ram_avail = 1000;

        for apparent in [
            0,
            cfg.rclone.fake_size_threshold,
            cfg.rclone.fake_size_threshold + 1,
        ] {
            snap.cache_details[0].apparent = apparent;
            let out = render_report(
                "/x",
                &cfg,
                &snap,
                &[],
                &snap.mount_parents,
                &OnceOpts {
                    show_boot: true,
                    cloud_size: true,
                    only_volumes: false,
                    only_folders: false,
                },
            );
            assert!(out.contains("RAM total=1.50 KB"), "{out}");
            assert!(out.contains("/mnt/g [rclone] total=— used=—"), "{out}");
        }
    }
}
