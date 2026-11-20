//! Identificação dinâmica de discos: `lsblk` (disco físico por partição)
//! e `zpool` (números reais de pools ZFS).
//!
//! - `lsblk -J -b -o NAME,PATH,PKNAME,TYPE` dá a árvore de block devices:
//!   partição/crypt/LUKS mapeiam para o disco pai (top-level `type=disk`);
//! - `zpool list -Hp -o name,size,alloc,free` dá total/used/free reais por pool;
//! - comando ausente ou com erro → [`DiskMap`] vazio: o grupo passa a ser a
//!   própria string do device (comportamento anterior, degradação graciosa);
//! - agrupamento ZFS por prefixo do dataset (`lpool/dados` → `lpool`) é puro
//!   e independe do lsblk (funciona mesmo sem os comandos).

use std::collections::HashMap;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;

/// Estatísticas reais de um pool ZFS (`zpool list -Hp`, bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZpoolStats {
    pub total: u64,
    pub used: u64,
    pub free: u64,
}

/// Mapa device → grupo de consolidação (disco físico ou pool ZFS).
#[derive(Debug, Clone, Default)]
pub struct DiskMap {
    /// `/dev/...` → id do grupo (nome do disco, ex. `sda`).
    group_of: HashMap<String, String>,
    /// Nome do pool ZFS → estatísticas reais.
    pools: HashMap<String, ZpoolStats>,
}

impl DiskMap {
    /// Construtor p/ testes (injeta fixtures sem rodar comandos).
    pub fn from_parts(
        group_of: HashMap<String, String>,
        pools: HashMap<String, ZpoolStats>,
    ) -> Self {
        Self { group_of, pools }
    }

    pub fn pool_stats(&self, group: &str) -> Option<ZpoolStats> {
        self.pools.get(group).copied()
    }

    /// Grupo de consolidação de um mount local:
    /// 1. fstype `zfs` → pool = segmento antes da primeira `/`;
    /// 2. device presente no lsblk → disco físico pai;
    /// 3. macOS sem lsblk: slices do mesmo disco APFS (`/dev/disk3s1`,
    ///    `/dev/disk3s3s1`) → container `disk3`;
    /// 4. fallback → própria string do device (status quo).
    pub fn group_for_device(&self, device: &str, fstype: &str) -> String {
        if fstype.eq_ignore_ascii_case("zfs") {
            if let Some(pool) = device.split('/').next() {
                if !pool.is_empty() {
                    return pool.to_string();
                }
            }
        }
        if let Some(group) = self.group_of.get(device) {
            return group.clone();
        }
        if let Some(rest) = device.strip_prefix("/dev/disk") {
            let digits: &str = rest
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap_or("");
            if !digits.is_empty() && rest[digits.len()..].starts_with('s') {
                return format!("disk{digits}");
            }
        }
        device.to_string()
    }
}

/// Cache TTL do [`DiskMap`]: layout de discos muda raramente; evita rodar
/// `lsblk`/`zpool` a cada tick de coleta.
#[derive(Debug)]
pub struct DiskMapCache {
    ttl: Duration,
    command_timeout: Duration,
    cached: Mutex<Option<(Instant, std::sync::Arc<DiskMap>)>>,
}

impl DiskMapCache {
    pub fn new(ttl: Duration, command_timeout: Duration) -> Self {
        Self {
            ttl,
            command_timeout,
            cached: Mutex::new(None),
        }
    }

    /// Mapa válido (cache TTL ou descoberta nova via `spawn_blocking`).
    pub async fn get(&self) -> std::sync::Arc<DiskMap> {
        if let Some((at, map)) = self.cached.lock().expect("diskmap cache").clone() {
            if at.elapsed() < self.ttl {
                return map;
            }
        }
        let timeout = self.command_timeout;
        let discovered = tokio::time::timeout(timeout, tokio::task::spawn_blocking(discover))
            .await
            .ok()
            .and_then(|res| res.ok())
            .unwrap_or_default();
        let map = std::sync::Arc::new(discovered);
        *self.cached.lock().expect("diskmap cache") = Some((Instant::now(), map.clone()));
        map
    }
}

/// Roda `lsblk` + `zpool` e monta o mapa. Nunca falha: ausência/erro de
/// comando deixa o mapa vazio (grupo = device).
pub fn discover() -> DiskMap {
    let group_of = Command::new("lsblk")
        .args(["-J", "-b", "-o", "NAME,PATH,PKNAME,TYPE"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_lsblk_json(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    let pools = Command::new("zpool")
        .args(["list", "-Hp", "-o", "name,size,alloc,free"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_zpool_list(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    DiskMap { group_of, pools }
}

#[derive(Debug, Deserialize)]
struct LsblkOut {
    #[serde(default)]
    blockdevices: Vec<LsblkDev>,
}

#[derive(Debug, Deserialize)]
struct LsblkDev {
    name: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(rename = "type")]
    dev_type: Option<String>,
    #[serde(default)]
    children: Option<Vec<LsblkDev>>,
}

/// Parse do JSON do lsblk: todo nó do subtree de um top-level `type=disk`
/// mapeia para o nome desse disco (partições, LUKS/crypt, md aninhado).
/// Nós fora de um disco (ex. `md0` top-level, `loop0`) mapeiam para si.
pub fn parse_lsblk_json(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(tree) = serde_json::from_str::<LsblkOut>(text) else {
        return out;
    };
    for dev in tree.blockdevices {
        let anchor = if dev.dev_type.as_deref() == Some("disk") {
            Some(dev.name.clone())
        } else {
            None
        };
        walk_lsblk(&dev, anchor, &mut out);
    }
    out
}

fn walk_lsblk(node: &LsblkDev, anchor: Option<String>, out: &mut HashMap<String, String>) {
    let anchor = match anchor {
        Some(a) => Some(a),
        None if node.dev_type.as_deref() == Some("disk") => Some(node.name.clone()),
        None => None,
    };
    // Sem ancora de disco: o nó é o próprio grupo (comportamento device).
    let group = anchor.clone().unwrap_or_else(|| node.name.clone());
    if let Some(path) = &node.path {
        out.insert(path.clone(), group.clone());
    }
    out.insert(format!("/dev/{}", node.name), group);
    for child in node.children.iter().flatten() {
        walk_lsblk(child, anchor.clone(), out);
    }
}

/// Parse de `zpool list -Hp -o name,size,alloc,free` (tabulado, bytes).
pub fn parse_zpool_list(text: &str) -> HashMap<String, ZpoolStats> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let name = it.next()?.to_string();
            let total = it.next()?.parse().ok()?;
            let used = it.next()?.parse().ok()?;
            let free = it.next()?.parse().ok()?;
            Some((name, ZpoolStats { total, used, free }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> (HashMap<String, String>, HashMap<String, ZpoolStats>) {
        let lsblk = r#"{
            "blockdevices": [
                {"name":"sda","path":"/dev/sda","type":"disk","children":[
                    {"name":"sda1","path":"/dev/sda1","type":"part"},
                    {"name":"sda2","path":"/dev/sda2","type":"part","children":[
                        {"name":"luks-root","path":"/dev/mapper/luks-root","type":"crypt"}
                    ]}
                ]},
                {"name":"nvme0n1","path":"/dev/nvme0n1","type":"disk","children":[
                    {"name":"nvme0n1p1","path":"/dev/nvme0n1p1","type":"part"},
                    {"name":"nvme0n1p2","path":"/dev/nvme0n1p2","type":"part"}
                ]},
                {"name":"md0","path":"/dev/md0","type":"md","children":[
                    {"name":"sdb1","path":"/dev/sdb1","type":"part"}
                ]},
                {"name":"loop0","path":"/dev/loop0","type":"loop"}
            ]
        }"#;
        let pools = [(
            "lpool".to_string(),
            ZpoolStats {
                total: 100,
                used: 40,
                free: 60,
            },
        )]
        .into_iter()
        .collect();
        (parse_lsblk_json(lsblk), pools)
    }

    #[test]
    fn lsblk_maps_partitions_and_crypt_to_parent_disk() {
        let (m, _) = fixtures();
        assert_eq!(m["/dev/sda1"], "sda");
        assert_eq!(m["/dev/sda2"], "sda");
        assert_eq!(m["/dev/mapper/luks-root"], "sda");
        assert_eq!(m["/dev/nvme0n1p1"], "nvme0n1");
        assert_eq!(m["/dev/nvme0n1p2"], "nvme0n1");
    }

    #[test]
    fn lsblk_non_disk_top_level_maps_to_itself() {
        let (m, _) = fixtures();
        assert_eq!(m["/dev/md0"], "md0");
        assert_eq!(m["/dev/loop0"], "loop0");
    }

    #[test]
    fn lsblk_garbage_yields_empty_map() {
        assert!(parse_lsblk_json("not json").is_empty());
        assert!(parse_lsblk_json("").is_empty());
    }

    #[test]
    fn zfs_devices_group_by_pool_prefix() {
        let (m, _) = fixtures();
        let m = DiskMap::from_parts(m, HashMap::new());
        assert_eq!(m.group_for_device("lpool/dados", "zfs"), "lpool");
        assert_eq!(m.group_for_device("lpool", "zfs"), "lpool");
        assert_eq!(m.group_for_device("rpool/ROOT/cachyos", "zfs"), "rpool");
    }

    #[test]
    fn group_for_device_falls_back_to_device_string() {
        let (m, _) = fixtures();
        let m = DiskMap::from_parts(m, HashMap::new());
        assert_eq!(m.group_for_device("server:/exp", "nfs4"), "server:/exp");
        assert_eq!(m.group_for_device("/dev/sda1", "ext4"), "sda");
        let empty = DiskMap::default();
        assert_eq!(empty.group_for_device("/dev/xyz1", "ext4"), "/dev/xyz1");
    }

    #[test]
    fn macos_disk_slices_group_by_container() {
        let empty = DiskMap::default();
        assert_eq!(empty.group_for_device("/dev/disk3s1", "apfs"), "disk3");
        assert_eq!(empty.group_for_device("/dev/disk3s3s1", "apfs"), "disk3");
        assert_eq!(empty.group_for_device("/dev/disk18s1", "apfs"), "disk18");
        // Disco inteiro (sem slice) não agrupa.
        assert_eq!(empty.group_for_device("/dev/disk3", "apfs"), "/dev/disk3");
        // Com lsblk (Linux), o mapa do lsblk vence o padrão macOS.
        let (m, _) = fixtures();
        let m = DiskMap::from_parts(m, HashMap::new());
        assert_eq!(m.group_for_device("/dev/sda1", "ext4"), "sda");
    }

    #[test]
    fn zpool_stats_lookup() {
        let (_, pools) = fixtures();
        let map = DiskMap::from_parts(HashMap::new(), pools);
        assert_eq!(
            map.pool_stats("lpool"),
            Some(ZpoolStats {
                total: 100,
                used: 40,
                free: 60
            })
        );
        assert_eq!(map.pool_stats("rpool"), None);
    }

    #[test]
    fn zpool_parser_skips_garbage_lines() {
        let text = "lpool\t8000\t4000\t4000\nbad line\nrpool\t100\t50\t50\n\n";
        let pools = parse_zpool_list(text);
        assert_eq!(pools.len(), 2);
        assert_eq!(pools["lpool"].total, 8000);
        assert_eq!(pools["rpool"].free, 50);
    }

    #[test]
    fn discover_smoke_never_fails() {
        let map = discover();
        // Ambiente-dependente (lsblk/zpool podem faltar): só garante que roda.
        let _ = map.group_for_device("/dev/qualquer", "ext4");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn lsblk_parser_never_panics(s in "\\PC{0,300}") {
            let _ = parse_lsblk_json(&s);
        }

        #[test]
        fn zpool_parser_never_panics(s in "\\PC{0,300}") {
            let _ = parse_zpool_list(&s);
        }

        #[test]
        fn group_for_device_never_panics(dev in "[a-zA-Z0-9_/.:-]{0,30}", fst in "[a-zA-Z0-9]{0,10}") {
            let m = DiskMap::default();
            let _ = m.group_for_device(&dev, &fst);
        }
    }
}
