//! Mounts (`/proc/mounts`), classificação local/remoto e `is_fake_size`.
//!
//! Port das funções `read_mounts`, `is_remote_fs`, `is_interesting_partition`,
//! `is_fake_size` e `get_mountpoint_of_path` de `disk_usage.py`.

use crate::config::Partitions;

/// Uma linha de montagem: (device, mountpoint, fstype).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub device: String,
    pub mountpoint: String,
    pub fstype: String,
}

/// Lê `/proc/mounts` (Linux) ou `mount` (macOS). Nunca falha: fallback `Root /`.
pub fn read_mounts() -> Vec<Mount> {
    if let Ok(text) = std::fs::read_to_string("/proc/mounts") {
        return parse_proc_mounts(&text);
    }
    match read_mounts_via_mount_cmd() {
        out if out.is_empty() => vec![Mount {
            device: "Root".to_string(),
            mountpoint: "/".to_string(),
            fstype: String::new(),
        }],
        out => out,
    }
}

/// Parse de `/proc/mounts` (device, mountpoint, fstype por linha).
pub fn parse_proc_mounts(text: &str) -> Vec<Mount> {
    let mut mounts = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 3 {
            mounts.push(Mount {
                device: parts[0].to_string(),
                mountpoint: unescape_mount(parts[1]),
                fstype: parts[2].to_string(),
            });
        }
    }
    mounts
}

fn unescape_mount(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\\\", "\\")
}

fn read_mounts_via_mount_cmd() -> Vec<Mount> {
    let out = std::process::Command::new("mount").output();
    let Ok(out) = out else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    parse_mount_output(&String::from_utf8_lossy(&out.stdout))
}

fn parse_mount_output(text: &str) -> Vec<Mount> {
    let mut mounts = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((dev, rest)) = line.split_once(" on ") {
            let device = dev.trim().to_string();
            let (mp, fstype) = match rest.split_once(" (") {
                Some((mp, tail)) => (
                    mp.trim().to_string(),
                    tail.split(',')
                        .next()
                        .unwrap_or("")
                        .trim_end_matches(')')
                        .to_string(),
                ),
                None => (rest.trim().to_string(), String::new()),
            };
            mounts.push(Mount {
                device,
                mountpoint: mp,
                fstype,
            });
        } else {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 3 {
                mounts.push(Mount {
                    device: parts[0].to_string(),
                    mountpoint: parts[2].to_string(),
                    fstype: parts[1].to_string(),
                });
            }
        }
    }
    mounts
}

/// Identifica montagens rclone / FUSE de nuvem (port de `is_remote_fs`).
pub fn is_remote_fs(device: &str, fstype: &str, remote_markers: &[String]) -> bool {
    let fstype_l = fstype.to_lowercase();
    let device_l = device.to_lowercase();
    if remote_markers
        .iter()
        .any(|m| fstype_l.contains(&m.to_lowercase()) || device_l.contains(&m.to_lowercase()))
    {
        return true;
    }
    if fstype_l.starts_with("fuse") && device.contains(':') && !device.starts_with('/') {
        return true;
    }
    false
}

/// True quando o backend não reporta cota real (port de `is_fake_size`).
pub fn is_fake_size(total: u64, free: u64, threshold: u64) -> bool {
    total >= threshold || free >= threshold || total == 0
}

/// Classifica partição em (remote, local). Port de `is_interesting_partition`.
pub fn is_interesting_partition(
    device: &str,
    fstype: &str,
    mountpoint: &str,
    p: &Partitions,
) -> (bool, bool) {
    let remote = is_remote_fs(device, fstype, &p.remote_markers);
    let fstype_l = fstype.to_lowercase();
    let local = p.device_match.iter().any(|m| device.contains(m))
        || p.mountpoints.iter().any(|m| mountpoint == m)
        || p.nfs_prefixes
            .iter()
            .any(|n| fstype_l.starts_with(&n.to_lowercase()));
    (remote, local)
}

/// Ponto de montagem no qual o caminho reside (port de `get_mountpoint_of_path`).
pub fn mountpoint_of_path(path: &str) -> String {
    let mut p = std::path::PathBuf::from(path);
    if let Ok(abs) = std::path::absolute(&p) {
        p = abs;
    }
    loop {
        if is_mount_point(&p) {
            return p.to_string_lossy().into_owned();
        }
        match p.parent() {
            Some(parent) if parent != p => p = parent.to_path_buf(),
            _ => return p.to_string_lossy().into_owned(),
        }
    }
}

fn is_mount_point(p: &std::path::Path) -> bool {
    // `parent_dev != dev` indica fronteira de mount.
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return false;
    };
    let dev = meta.dev();
    match p.parent() {
        Some(parent) => match std::fs::symlink_metadata(parent) {
            Ok(pm) => pm.dev() != dev,
            Err(_) => true,
        },
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Partitions;

    fn parts() -> Partitions {
        Partitions::default()
    }

    #[test]
    fn parses_proc_mounts() {
        let text = "/dev/nvme0n1p2 / btrfs rw,noatime 0 0\ngdrive: /mnt/g fuse.rclone rw 0 0\n";
        let m = parse_proc_mounts(text);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].mountpoint, "/");
        assert_eq!(m[1].fstype, "fuse.rclone");
    }

    #[test]
    fn unescapes_spaces() {
        let text = "/dev/sda1 /mnt/My\\040Disk ext4 rw 0 0\n";
        let m = parse_proc_mounts(text);
        assert_eq!(m[0].mountpoint, "/mnt/My Disk");
    }

    #[test]
    fn remote_detection_matches_python() {
        let p = parts();
        assert!(is_interesting_partition("gdrive:", "fuse.rclone", "/mnt/g", &p).0);
        assert!(is_interesting_partition("rclone-crypt:", "fuse", "/mnt/c", &p).0);
        assert!(is_interesting_partition("gdrive:media", "fuse.rclone", "/mnt/m", &p).0);
        assert!(!is_interesting_partition("/dev/sda1", "ext4", "/mnt/x", &p).0);
    }

    #[test]
    fn local_detection_matches_python() {
        let p = parts();
        assert!(is_interesting_partition("/dev/sda1", "ext4", "/mnt/x", &p).1);
        assert!(is_interesting_partition("nfs-server:/exp", "nfs4", "/mnt/n", &p).1);
        assert!(is_interesting_partition("tmpfs", "tmpfs", "/", &p).1);
        assert!(!is_interesting_partition("sysfs", "sysfs", "/sys", &p).1);
    }

    #[test]
    fn fake_size_matches_python() {
        let t = 1u64 << 50;
        assert!(is_fake_size(t, 0, t));
        assert!(is_fake_size(t + 1, 5, t));
        assert!(is_fake_size(0, 0, t));
        assert!(!is_fake_size(100, 50, t));
    }

    #[test]
    fn mountpoint_of_tmp_is_filesystem_root_or_tmp() {
        let mp = mountpoint_of_path("/tmp");
        assert!(!mp.is_empty());
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn proc_mounts_never_panics(s in "\\PC{0,200}") {
            let _ = parse_proc_mounts(&s);
        }

        #[test]
        fn fake_size_total_zero_always_fake(free in any::<u64>(), t in 1u64..u64::MAX) {
            prop_assert!(is_fake_size(0, free, t));
        }

        #[test]
        fn fake_size_below_threshold_not_fake(total in 1u64..1000, free in 1u64..1000) {
            prop_assert!(!is_fake_size(total, free, 1u64 << 50));
        }
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    #[test]
    fn read_mounts_lists_root_on_linux() {
        let m = read_mounts();
        assert!(!m.is_empty(), "algum mount sempre existe");
        assert!(m.iter().any(|x| x.mountpoint == "/"));
    }

    #[test]
    fn parses_macos_mount_output_with_apfs_autofs_and_spaces() {
        let output = concat!(
            "/dev/disk3s1 on / (apfs, sealed, local, read-only, journaled)\n",
            "/dev/disk3s5 on /System/Volumes/Data (apfs, local, journaled)\n",
            "/dev/disk4s1 on /Volumes/External Disk (apfs, local, journaled)\n",
            "map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)\n",
        );
        let mounts = parse_mount_output(output);

        assert_eq!(mounts.len(), 4);
        assert_eq!(mounts[0].device, "/dev/disk3s1");
        assert_eq!(mounts[0].mountpoint, "/");
        assert_eq!(mounts[0].fstype, "apfs");
        assert_eq!(mounts[2].mountpoint, "/Volumes/External Disk");
        assert_eq!(mounts[3].device, "map auto_home");
        assert_eq!(mounts[3].fstype, "autofs");
    }

    #[test]
    fn parses_linux_mount_output_fallback_and_ignores_invalid_lines() {
        let output = concat!(
            "/dev/sdb on /Volumes/plain\n",
            "server:/export nfs4 /mnt/share rw,relatime 0 0\n",
            "malformed\n",
            "\n",
        );
        let mounts = parse_mount_output(output);

        assert_eq!(
            mounts,
            vec![
                Mount {
                    device: "/dev/sdb".into(),
                    mountpoint: "/Volumes/plain".into(),
                    fstype: String::new(),
                },
                Mount {
                    device: "server:/export".into(),
                    mountpoint: "/mnt/share".into(),
                    fstype: "nfs4".into(),
                },
            ]
        );
    }

    #[test]
    fn parses_proc_mounts_all_escapes_and_skips_malformed_lines() {
        let input = concat!(
            "incomplete /mnt/ignored\n",
            "/dev/sda /mnt/a\\040b\\011c\\012d\\\\e ext4 rw 0 0\n",
        );

        let mounts = parse_proc_mounts(input);

        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mountpoint, "/mnt/a b\tc\nd\\e");
    }

    #[test]
    fn mountpoint_and_ismount_helpers() {
        assert!(!mountpoint_of_path("/tmp").is_empty());
        assert!(!mountpoint_of_path("/tmp/definitivamente-nao-mount").is_empty());
        assert!(is_mount_point(&std::path::PathBuf::from("/")));
        assert!(!is_mount_point(&std::path::PathBuf::from(
            "/nonexistent-du-rs-mp"
        )));
    }
}
