//! Coleta de RAM (sysinfo) + volumes (sysinfo, skip FUSE) + agregação.
//!
//! Port de `collect_system_stats_loop` / `collect_system_stats_once`:
//! - RAM/swap via `sysinfo::System` (bytes);
//! - discos via `sysinfo::Disks`, mas entradas `fuse.rclone` NÃO usam
//!   statvfs (marcadas fake, igual `is_fake_size` do Python);
//! - refresh de discos com timeout: em timeout reusa números anteriores
//!   p/ nunca travar o tick de coleta;
//! - agregação FÍSICO/NUVEM/VIRTUAL + cache rclone idêntica ao Python.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{AppConfig, Partitions};
use crate::disks::DiskMap;
use crate::mounts::{is_fake_size, is_interesting_partition, read_mounts};
use crate::rclone::parent_remote;
use crate::ssh::{RemotePartition, SshClient};

#[derive(Debug, Clone, Default)]
pub struct VolumeUsage {
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub fake: bool,
}

#[derive(Debug, Clone)]
pub struct PartitionView {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
    pub remote: bool,
}

/// Volume local consolidado (disco físico ou pool ZFS) p/ exibição.
#[derive(Debug, Clone)]
pub struct DiskGroupView {
    pub label: String,
    pub mountpoints: Vec<String>,
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

#[derive(Debug, Clone, Default)]
pub struct CacheDetail {
    pub name: String,
    pub allocated: u64,
    pub apparent: u64,
}

#[derive(Debug, Clone, Default)]
pub struct SystemSnapshot {
    pub ram_total: u64,
    pub ram_used: u64,
    pub ram_avail: u64,
    pub ram_pct: f64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub swap_free: u64,
    pub swap_pct: f64,
    pub partitions: Vec<PartitionView>,
    pub usages: HashMap<String, VolumeUsage>,
    /// Volumes locais consolidados por disco/pool (vazio em group_by="device").
    pub disk_groups: Vec<DiskGroupView>,
    pub mount_parents: HashMap<String, String>,
    pub remotes_parent_count: HashMap<String, usize>,
    pub parent_totals: HashMap<String, u64>,
    pub parent_used_sum: HashMap<String, u64>,
    pub total_physical: u64,
    pub used_physical: u64,
    pub free_physical: u64,
    pub total_cloud: u64,
    pub used_cloud: u64,
    pub free_cloud: u64,
    pub total_virtual: u64,
    pub used_virtual: u64,
    pub free_virtual: u64,
    pub rclone_cache_total: u64,
    pub cache_details: Vec<CacheDetail>,
    pub remote_hosts: HashMap<String, Vec<RemotePartition>>,
    /// Hosts SSH que falharam na última coleta (host → motivo legível).
    pub ssh_errors: HashMap<String, String>,
}

pub struct RamInfo {
    pub total: u64,
    pub avail: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

fn read_ram() -> RamInfo {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    RamInfo {
        total: sys.total_memory(),
        avail: sys.available_memory(),
        swap_total: sys.total_swap(),
        swap_free: sys.free_swap(),
    }
}

struct DiskUsage {
    mountpoint: String,
    total: u64,
    avail: u64,
}

/// Discos via sysinfo com timeout; fstype vem de `/proc/mounts` (autoritativo).
/// Em timeout, retorna `None` e o chamador reusa o snapshot anterior.
async fn read_disk_usages(timeout: Duration) -> Option<Vec<DiskUsage>> {
    let run = tokio::task::spawn_blocking(|| {
        let disks = sysinfo::Disks::new_with_refreshed_list();
        disks
            .list()
            .iter()
            .map(|d| DiskUsage {
                mountpoint: d.mount_point().to_string_lossy().into_owned(),
                total: d.total_space(),
                avail: d.available_space(),
            })
            .collect::<Vec<_>>()
    });
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok(v)) => Some(v),
        _ => None,
    }
}

/// Fallback do mapa de usos quando o refresh do sysinfo falha/estoura o
/// timeout: reaproveita o snapshot anterior por mount (total/free). Sem
/// `prev`, mapa vazio — nada é inventado.
fn sys_map_fallback(prev: Option<&SystemSnapshot>) -> HashMap<String, (u64, u64)> {
    prev.map(|p| {
        p.usages
            .iter()
            .map(|(k, u)| (k.clone(), (u.total, u.free)))
            .collect()
    })
    .unwrap_or_default()
}

/// Tamanhos do cache VFS por remote (walkdir, same-filesystem, sem FUSE).
/// Usa `walkdir` com `same_file_system(true)` (xdev) — walk nativo, sem `du`.
pub fn cache_dir_sizes(cache_base: &Path, per_dir_timeout: Duration) -> Vec<CacheDetail> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(cache_base) else {
        return out;
    };
    // Coleta subdirs primeiro (I/O rápido), depois walk com timeout cada.
    let mut subs: Vec<(String, PathBuf)> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            subs.push((e.file_name().to_string_lossy().into_owned(), p));
        }
    }
    subs.sort_by(|a, b| a.0.cmp(&b.0));
    let cancel = tokio_util::sync::CancellationToken::new();
    for (name, path) in subs {
        let deadline = std::time::Instant::now() + per_dir_timeout;
        // Walk nativo via walkdir (xdev) somando apparent + allocated.
        let mut apparent = 0u64;
        let mut allocated = 0u64;
        let mut ok = true;
        for entry in walkdir::WalkDir::new(&path)
            .follow_links(false)
            .same_file_system(true)
            .into_iter()
            .filter_map(Result::ok)
        {
            if std::time::Instant::now() > deadline || cancel.is_cancelled() {
                ok = false;
                break;
            }
            let meta = match std::fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(_) => continue,
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                apparent += meta.size();
                allocated += meta.blocks() * 512;
            }
            #[cfg(not(unix))]
            {
                apparent += meta.len();
                allocated += meta.len();
            }
        }
        if ok && allocated > 0 {
            out.push(CacheDetail {
                name,
                allocated,
                apparent,
            });
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.allocated));
    out
}

#[allow(clippy::too_many_arguments)]
pub async fn collect<F>(
    cfg: &AppConfig,
    prev: Option<&SystemSnapshot>,
    ssh: &SshClient,
    hosts: &[String],
    remotes_map: &HashMap<String, String>,
    ssh_filter: &F,
    disks: &DiskMap,
) -> SystemSnapshot
where
    F: Fn(&str, &str, &str) -> (bool, bool) + Send + Sync,
{
    // 1. RAM
    let ram = read_ram();
    let ram_used = ram.total.saturating_sub(ram.avail);
    let ram_pct = pct(ram_used, ram.total);
    let swap_used = ram.swap_total.saturating_sub(ram.swap_free);

    // 2. Volumes
    let mounts = read_mounts();
    let sys_usages = read_disk_usages(Duration::from_secs_f64(cfg.timeouts.sysinfo_secs)).await;
    let sys_map: HashMap<String, (u64, u64)> = match sys_usages {
        Some(v) => v
            .into_iter()
            .map(|d| (d.mountpoint, (d.total, d.avail)))
            .collect(),
        None => sys_map_fallback(prev),
    };

    let mut partitions: Vec<PartitionView> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for m in &mounts {
        let (remote, local) =
            is_interesting_partition(&m.device, &m.fstype, &m.mountpoint, &cfg.partitions);
        if !remote && !local {
            continue;
        }
        if !std::path::Path::new(&m.mountpoint).is_dir() || !seen.insert(m.mountpoint.clone()) {
            continue;
        }
        partitions.push(PartitionView {
            device: m.device.clone(),
            mountpoint: m.mountpoint.clone(),
            fstype: m.fstype.clone(),
            remote,
        });
    }

    let fuse_skip = |fstype: &str| {
        let f = fstype.to_lowercase();
        f == "fuse.rclone"
            || cfg
                .partitions
                .remote_markers
                .iter()
                .any(|m| f.contains(&m.to_lowercase()))
    };

    let mut usages: HashMap<String, VolumeUsage> = HashMap::new();
    for p in &partitions {
        if p.remote && fuse_skip(&p.fstype) {
            // Skip statvfs em FUSE rclone: sem cota confiável (fake).
            usages.insert(
                p.mountpoint.clone(),
                VolumeUsage {
                    total: 0,
                    used: 0,
                    free: 0,
                    fake: true,
                },
            );
            continue;
        }
        if let Some((total, avail)) = sys_map.get(&p.mountpoint) {
            let used = total.saturating_sub(*avail);
            usages.insert(
                p.mountpoint.clone(),
                VolumeUsage {
                    total: *total,
                    used,
                    free: *avail,
                    fake: is_fake_size(*total, *avail, cfg.rclone.fake_size_threshold),
                },
            );
        }
    }

    // Pais remotos + cache
    let mut mount_parents: HashMap<String, String> = HashMap::new();
    let mut remotes_parent_count: HashMap<String, usize> = HashMap::new();
    for p in &partitions {
        if p.remote {
            let parent = parent_remote(&p.device, remotes_map);
            mount_parents.insert(p.mountpoint.clone(), parent.clone());
            *remotes_parent_count.entry(parent).or_default() += 1;
        }
    }

    let cache_base = crate::rclone::expand_tilde(&cfg.rclone.cache_base);
    let walk_cache_t = Duration::from_secs_f64(cfg.timeouts.walk_cache_secs);
    // Não bloqueia o executor: walk síncrono vai p/ spawn_blocking.
    let cache_details =
        tokio::task::spawn_blocking(move || cache_dir_sizes(&cache_base, walk_cache_t))
            .await
            .unwrap_or_default();
    let rclone_cache_total: u64 = cache_details.iter().map(|c| c.allocated).sum();

    let mut parent_used_sum: HashMap<String, u64> = HashMap::new();
    let mut parent_totals: HashMap<String, u64> = HashMap::new();
    for p in &partitions {
        let Some(u) = usages.get(&p.mountpoint) else {
            continue;
        };
        if p.remote {
            if let Some(parent) = mount_parents.get(&p.mountpoint) {
                if !u.fake {
                    *parent_used_sum.entry(parent.clone()).or_default() += u.used;
                    let e = parent_totals.entry(parent.clone()).or_default();
                    *e = (*e).max(u.total);
                }
            }
        }
    }

    // Remotos: consolidação por remote pai rclone (port do Python).
    struct RemoteAgg {
        total: u64,
        used: u64,
        free: u64,
        fake: bool,
    }
    let mut remote_unique: HashMap<String, RemoteAgg> = HashMap::new();
    for p in &partitions {
        if !p.remote {
            continue;
        }
        let Some(u) = usages.get(&p.mountpoint) else {
            continue;
        };
        let parent = mount_parents
            .get(&p.mountpoint)
            .cloned()
            .unwrap_or_default();
        match remote_unique.get_mut(&parent) {
            Some(_) => {}
            None => {
                remote_unique.insert(
                    parent.clone(),
                    RemoteAgg {
                        total: if u.fake {
                            0
                        } else {
                            parent_totals.get(&parent).copied().unwrap_or(u.total)
                        },
                        used: if u.fake {
                            0
                        } else {
                            parent_used_sum.get(&parent).copied().unwrap_or(u.used)
                        },
                        free: if u.fake {
                            0
                        } else {
                            parent_totals
                                .get(&parent)
                                .copied()
                                .unwrap_or(u.total)
                                .saturating_sub(
                                    parent_used_sum.get(&parent).copied().unwrap_or(u.used),
                                )
                        },
                        fake: u.fake,
                    },
                );
            }
        }
    }

    // Locais: 1ª-vence por device, depois consolida por disco físico/pool
    // ZFS (`group_by = "disk"`) ou fica por device (`group_by = "device"`).
    let group_by_disk = cfg.partitions.group_by == "disk";
    let locals = consolidate_locals(&partitions, &usages, disks, group_by_disk);
    let mut total_physical = 0u64;
    let (mut used_physical, mut free_physical) = (0u64, 0u64);
    for g in &locals {
        total_physical = total_physical.saturating_add(g.total);
        used_physical = used_physical.saturating_add(g.used);
        free_physical = free_physical.saturating_add(g.free);
    }
    let (mut total_cloud, mut used_cloud, mut free_cloud) = (0u64, 0u64, 0u64);
    let (mut total_virtual, mut used_virtual, mut free_virtual) =
        (total_physical, used_physical, free_physical);
    for a in remote_unique.values() {
        if !a.fake {
            total_cloud = total_cloud.saturating_add(a.total);
            used_cloud = used_cloud.saturating_add(a.used);
            free_cloud = free_cloud.saturating_add(a.free);
            total_virtual = total_virtual.saturating_add(a.total);
            used_virtual = used_virtual.saturating_add(a.used);
            free_virtual = free_virtual.saturating_add(a.free);
        }
    }
    let disk_groups = if group_by_disk { locals } else { Vec::new() };

    // 3. SSH (cache TTL próprio; hosts vazios = sem painel; falhas ficam
    // visíveis em ssh_errors e re-tentam a cada tick, sem entrar no cache).
    let fetch = ssh.fetch_all(hosts, ssh_filter).await;

    SystemSnapshot {
        ram_total: ram.total,
        ram_used,
        ram_avail: ram.avail,
        ram_pct,
        swap_total: ram.swap_total,
        swap_used,
        swap_free: ram.swap_free,
        swap_pct: pct(swap_used, ram.swap_total),
        partitions,
        usages,
        disk_groups,
        mount_parents,
        remotes_parent_count,
        parent_totals,
        parent_used_sum,
        total_physical,
        used_physical,
        free_physical,
        total_cloud,
        used_cloud,
        free_cloud,
        total_virtual,
        used_virtual,
        free_virtual,
        rclone_cache_total,
        cache_details,
        remote_hosts: fetch.partitions,
        ssh_errors: fetch.errors,
    }
}

fn pct(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        used as f64 / total as f64 * 100.0
    }
}

/// Consolidação de volumes locais (pura, testável):
/// 1. colapso por device com "primeira ocorrência vence" (subvolumes btrfs e
///    binds do mesmo device não contam duas vezes);
/// 2. soma dos devices por grupo — disco físico (via lsblk) ou pool ZFS;
/// 3. pool com `zpool list` usa os números reais; dataset ZFS sem zpool usa
///    max(total)/max(used) (used de dataset pai inclui filhos — soma contaria
///    espaço repetido);
/// 4. fallback (sem grupo): o próprio device é o grupo (status quo).
pub fn consolidate_locals(
    partitions: &[PartitionView],
    usages: &HashMap<String, VolumeUsage>,
    disks: &DiskMap,
    group_by_disk: bool,
) -> Vec<DiskGroupView> {
    struct DevAgg {
        total: u64,
        used: u64,
        free: u64,
    }
    let mut by_device: HashMap<&str, DevAgg> = HashMap::new();
    let mut device_fstype: HashMap<&str, &str> = HashMap::new();
    let mut mps_by_group: HashMap<String, Vec<String>> = HashMap::new();
    for p in partitions {
        if p.remote {
            continue;
        }
        let Some(u) = usages.get(&p.mountpoint) else {
            continue;
        };
        device_fstype
            .entry(p.device.as_str())
            .or_insert(p.fstype.as_str());
        by_device.entry(p.device.as_str()).or_insert(DevAgg {
            total: u.total,
            used: u.used,
            free: u.free,
        });
        let group = if group_by_disk {
            disks.group_for_device(&p.device, p.fstype.as_str())
        } else {
            p.device.clone()
        };
        mps_by_group
            .entry(group)
            .or_default()
            .push(p.mountpoint.clone());
    }

    struct Fold {
        total: u64,
        used: u64,
        free: u64,
        max_total: u64,
        max_used: u64,
        /// FS de espaço compartilhado (zfs: used do pai inclui filhos;
        /// apfs: volumes do mesmo container reportam o total/free dele).
        shared_space: bool,
    }
    let mut folded: HashMap<String, Fold> = HashMap::new();
    for (dev, agg) in &by_device {
        let fst = device_fstype.get(dev).copied().unwrap_or("");
        let group = if group_by_disk {
            disks.group_for_device(dev, fst)
        } else {
            (*dev).to_string()
        };
        let e = folded.entry(group).or_insert(Fold {
            total: 0,
            used: 0,
            free: 0,
            max_total: 0,
            max_used: 0,
            shared_space: false,
        });
        e.total = e.total.saturating_add(agg.total);
        e.used = e.used.saturating_add(agg.used);
        e.free = e.free.saturating_add(agg.free);
        e.max_total = e.max_total.max(agg.total);
        e.max_used = e.max_used.max(agg.used);
        if fst.eq_ignore_ascii_case("zfs") || fst.eq_ignore_ascii_case("apfs") {
            e.shared_space = true;
        }
    }

    let mut out = Vec::with_capacity(folded.len());
    for (group, f) in folded {
        let (total, used, free) = if let Some(z) = disks.pool_stats(&group) {
            (z.total, z.used, z.free)
        } else if f.shared_space {
            (
                f.max_total,
                f.max_used,
                f.max_total.saturating_sub(f.max_used),
            )
        } else {
            (f.total, f.used, f.free)
        };
        let mut mountpoints = mps_by_group.remove(&group).unwrap_or_default();
        mountpoints.sort();
        mountpoints.dedup();
        out.push(DiskGroupView {
            label: group,
            mountpoints,
            total,
            used,
            free,
        });
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// Filtro SSH: mesma regra de partições, sem coluna fstype no `df -Pk`.
pub fn ssh_interesting(
    device: &str,
    _fstype: &str,
    mountpoint: &str,
    p: &Partitions,
) -> (bool, bool) {
    // `df -Pk` não traz fstype: classifica por device/mount (+ regra fuse genérica).
    let device_l = device.to_lowercase();
    let remote = p
        .remote_markers
        .iter()
        .any(|m| device_l.contains(&m.to_lowercase()))
        || (device.contains(':') && !device.starts_with('/'));
    let local = p.device_match.iter().any(|m| device.contains(m))
        || p.mountpoints.iter().any(|m| mountpoint == m);
    (remote, local)
}

/// Tamanho aparente do cache p/ `--cloud-size` (logical do cache em vez de "—").
pub fn cache_apparent_for(remote_name: &str, snap: &SystemSnapshot) -> Option<u64> {
    snap.cache_details
        .iter()
        .find(|c| c.name == remote_name)
        .map(|c| c.apparent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Partitions;

    #[test]
    fn ssh_filter_marks_remote_by_device() {
        let p = Partitions::default();
        assert!(ssh_interesting("gdrive:", "", "/mnt/g", &p).0);
        assert!(ssh_interesting("/dev/sda1", "", "/", &p).1);
        assert!(!ssh_interesting("tmpfs", "", "/run", &p).1);
    }

    #[test]
    fn cache_sizes_empty_for_missing_base() {
        let out = cache_dir_sizes(
            Path::new("/nonexistent-du-rs-cache"),
            Duration::from_secs(1),
        );
        assert!(out.is_empty());
    }

    #[test]
    fn cache_sizes_walk_local_dir() {
        let base = std::env::temp_dir().join(format!("du-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("remoteA")).unwrap();
        std::fs::write(base.join("remoteA").join("f.bin"), vec![9u8; 2048]).unwrap();
        let out = cache_dir_sizes(&base, Duration::from_secs(5));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "remoteA");
        assert!(out[0].apparent >= 2048);
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn sys_map_fallback_reuses_previous_when_refresh_fails() {
        // Refresh falho sem previous => mapa vazio (nada inventado).
        assert!(sys_map_fallback(None).is_empty());

        // Refresh falho com previous => valores do snapshot anterior
        // reaproveitados por mount. Chave sintética: nenhum host tem esse
        // mount, então o resultado não depende de /proc/mounts.
        let mut previous = SystemSnapshot::default();
        previous.usages.insert(
            "/nonexistent-zc-mount-probe".into(),
            VolumeUsage {
                total: 100,
                used: 75,
                free: 25,
                fake: false,
            },
        );
        let m = sys_map_fallback(Some(&previous));
        assert_eq!(m.get("/nonexistent-zc-mount-probe"), Some(&(100, 25)));
    }

    #[tokio::test]
    async fn collect_smoke_on_real_machine() {
        let cfg = AppConfig::default();
        let ssh = SshClient::new(2, Duration::from_secs(4), Duration::from_secs(30));
        let filter = cfg.partitions.clone();
        let disks = DiskMap::default();
        let snap = collect(
            &cfg,
            None,
            &ssh,
            &[],
            &HashMap::new(),
            &|d, f, m| ssh_interesting(d, f, m, &filter),
            &disks,
        )
        .await;
        assert!(snap.ram_total > 0, "sysinfo retorna RAM");
        assert!(!snap.partitions.is_empty(), "filtros acham /");
        assert!(snap.usages.contains_key("/"));
        assert!(
            !snap.disk_groups.is_empty(),
            "group_by=disk produz grupos locais"
        );
        // Reusa snapshot anterior quando o refresh falha (fallback path).
        let prev = snap.clone();
        let filter2 = cfg.partitions.clone();
        let snap2 = collect(
            &cfg,
            Some(&prev),
            &ssh,
            &[],
            &HashMap::new(),
            &|d, f, m| ssh_interesting(d, f, m, &filter2),
            &disks,
        )
        .await;
        assert!(snap2.ram_total > 0);
    }

    #[test]
    fn cache_apparent_lookup() {
        let mut snap = SystemSnapshot::default();
        snap.cache_details.push(CacheDetail {
            name: "g".to_string(),
            allocated: 10,
            apparent: 20,
        });
        assert_eq!(cache_apparent_for("g", &snap), Some(20));
        assert_eq!(cache_apparent_for("zz", &snap), None);
    }

    #[test]
    fn ssh_interesting_remote_markers() {
        let p = Partitions::default();
        // fuse genérico com `:` também é remoto (regra fuse do Python).
        assert!(ssh_interesting("data:", "", "/mnt/d", &p).0);
    }

    fn fixtures_consolidate() -> (Vec<PartitionView>, HashMap<String, VolumeUsage>) {
        let mut usages = HashMap::new();
        usages.insert(
            "/".to_string(),
            VolumeUsage {
                total: 100,
                used: 40,
                free: 60,
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
            "/home2".to_string(),
            VolumeUsage {
                total: 100,
                used: 40,
                free: 60,
                fake: false,
            },
        );
        usages.insert(
            "/dados".to_string(),
            VolumeUsage {
                total: 800,
                used: 300,
                free: 500,
                fake: false,
            },
        );
        usages.insert(
            "/media".to_string(),
            VolumeUsage {
                total: 800,
                used: 200,
                free: 600,
                fake: false,
            },
        );
        usages.insert(
            "/mnt/nfs".to_string(),
            VolumeUsage {
                total: 50,
                used: 10,
                free: 40,
                fake: false,
            },
        );
        let partitions = vec![
            PartitionView {
                device: "/dev/sda1".into(),
                mountpoint: "/".into(),
                fstype: "ext4".into(),
                remote: false,
            },
            PartitionView {
                device: "/dev/sda2".into(),
                mountpoint: "/boot".into(),
                fstype: "vfat".into(),
                remote: false,
            },
            // Subvolume btrfs: mesmo device, deve colapsar (1ª vence).
            PartitionView {
                device: "/dev/sda2".into(),
                mountpoint: "/home2".into(),
                fstype: "btrfs".into(),
                remote: false,
            },
            PartitionView {
                device: "lpool/dados".into(),
                mountpoint: "/dados".into(),
                fstype: "zfs".into(),
                remote: false,
            },
            PartitionView {
                device: "lpool/media".into(),
                mountpoint: "/media".into(),
                fstype: "zfs".into(),
                remote: false,
            },
            PartitionView {
                device: "server:/exp".into(),
                mountpoint: "/mnt/nfs".into(),
                fstype: "nfs4".into(),
                remote: false,
            },
        ];
        (partitions, usages)
    }

    fn test_diskmap() -> DiskMap {
        let group_of = ["/dev/sda1", "/dev/sda2"]
            .into_iter()
            .map(|d| (d.to_string(), "sda".to_string()))
            .collect();
        let pools = [(
            "lpool".to_string(),
            crate::disks::ZpoolStats {
                total: 1000,
                used: 450,
                free: 550,
            },
        )]
        .into_iter()
        .collect();
        DiskMap::from_parts(group_of, pools)
    }

    #[test]
    fn consolidate_locals_groups_by_disk_and_pool() {
        let (partitions, usages) = fixtures_consolidate();
        let disks = test_diskmap();
        let out = consolidate_locals(&partitions, &usages, &disks, true);
        assert_eq!(out.len(), 3, "sda + lpool + nfs device: {out:?}");
        let sda = out.iter().find(|g| g.label == "sda").expect("grupo sda");
        // sda1 (100/40/60) + sda2 (10/1/9); subvolume /home2 colapsa no device
        // (1ª vence no número) mas entra na lista de mountpoints do grupo.
        assert_eq!((sda.total, sda.used, sda.free), (110, 41, 69));
        assert_eq!(sda.mountpoints, vec!["/", "/boot", "/home2"]);
        let lp = out.iter().find(|g| g.label == "lpool").expect("pool lpool");
        // zpool list vence a soma de datasets.
        assert_eq!((lp.total, lp.used, lp.free), (1000, 450, 550));
        assert_eq!(lp.mountpoints, vec!["/dados", "/media"]);
        let nfs = out
            .iter()
            .find(|g| g.label == "server:/exp")
            .expect("nfs vira grupo próprio");
        assert_eq!(nfs.mountpoints, vec!["/mnt/nfs"]);
    }

    #[test]
    fn consolidate_locals_zfs_without_zpool_uses_max_not_sum() {
        let (partitions, usages) = fixtures_consolidate();
        let disks = DiskMap::default(); // sem lsblk/zpool
        let out = consolidate_locals(&partitions, &usages, &disks, true);
        let lp = out.iter().find(|g| g.label == "lpool").expect("pool lpool");
        // Sem zpool: used de dataset pai inclui filhos → max, não soma (1100).
        assert_eq!((lp.total, lp.used, lp.free), (800, 300, 500));
    }

    #[test]
    fn consolidate_locals_apfs_volumes_share_container_space() {
        // macOS: / e /System/Volumes/Data são do mesmo container APFS e
        // reportam o total/free dele — max, não soma (senão FÍSICO dobraria).
        let mut usages = HashMap::new();
        usages.insert(
            "/".to_string(),
            VolumeUsage {
                total: 926,
                used: 423,
                free: 503,
                fake: false,
            },
        );
        usages.insert(
            "/System/Volumes/Data".to_string(),
            VolumeUsage {
                total: 926,
                used: 423,
                free: 503,
                fake: false,
            },
        );
        usages.insert(
            "/Volumes/MSD512".to_string(),
            VolumeUsage {
                total: 476,
                used: 80,
                free: 396,
                fake: false,
            },
        );
        let partitions = vec![
            PartitionView {
                device: "/dev/disk3s3s1".into(),
                mountpoint: "/".into(),
                fstype: "apfs".into(),
                remote: false,
            },
            PartitionView {
                device: "/dev/disk3s1".into(),
                mountpoint: "/System/Volumes/Data".into(),
                fstype: "apfs".into(),
                remote: false,
            },
            PartitionView {
                device: "/dev/disk7s1".into(),
                mountpoint: "/Volumes/MSD512".into(),
                fstype: "apfs".into(),
                remote: false,
            },
        ];
        let out = consolidate_locals(&partitions, &usages, &DiskMap::default(), true);
        let d3 = out
            .iter()
            .find(|g| g.label == "disk3")
            .expect("container disk3");
        assert_eq!((d3.total, d3.used, d3.free), (926, 423, 503));
        assert_eq!(d3.mountpoints, vec!["/", "/System/Volumes/Data"]);
        let d7 = out.iter().find(|g| g.label == "disk7").expect("disk7");
        assert_eq!((d7.total, d7.used, d7.free), (476, 80, 396));
    }

    #[test]
    fn consolidate_locals_device_mode_keeps_per_device_rows() {
        let (partitions, usages) = fixtures_consolidate();
        let disks = test_diskmap();
        let out = consolidate_locals(&partitions, &usages, &disks, false);
        // sda1, sda2 (subvolume colapsa), lpool/dados, lpool/media, nfs.
        assert_eq!(out.len(), 5, "device mode não agrupa: {out:?}");
        assert!(out.iter().any(|g| g.label == "/dev/sda1"));
        assert!(out.iter().any(|g| g.label == "lpool/dados"));
        let btrfs = out
            .iter()
            .find(|g| g.label == "/dev/sda2")
            .expect("device único");
        assert_eq!(btrfs.mountpoints, vec!["/boot", "/home2"]);
        assert_eq!((btrfs.total, btrfs.used), (10, 1), "1ª ocorrência vence");
    }
}
