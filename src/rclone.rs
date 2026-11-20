//! Parser próprio do `rclone.conf` + redirect p/ cache VFS local.
//!
//! Port de `get_rclone_remotes_mapping`, `get_remote_name_from_device`,
//! `get_parent_remote`, `find_remote_mount_info` e `get_rclone_cache_path`.
//! Parser INI próprio (sem `configparser`): seções `[nome]`, chaves
//! case-insensitive, valores `remote = outro:path` resolvidos p/ o pai.
//! O walk NUNCA entra no FUSE: mede `cache_base/<remote>/...` como o Python.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::Rclone;
use crate::mounts::{is_remote_fs, Mount};

/// Expande `~` e `$HOME` no início do caminho.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(shellexpand_env(s))
}

fn shellexpand_env(s: &str) -> String {
    // Expansão mínima de `$VAR`/`${VAR}` sem dependência externa.
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' {
            if chars.peek() == Some(&'{') {
                chars.next();
                let mut name = String::new();
                for ch in chars.by_ref() {
                    if ch == '}' {
                        break;
                    }
                    name.push(ch);
                }
                if let Ok(v) = std::env::var(&name) {
                    out.push_str(&v);
                }
            } else {
                let mut name = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_alphanumeric() || ch == '_' {
                        name.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if name.is_empty() {
                    out.push('$');
                } else if let Ok(v) = std::env::var(&name) {
                    out.push_str(&v);
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Resolve a lista de caminhos candidatos do rclone.conf.
/// `$RCLONE_CONFIG` tem prioridade, depois `config_paths` do TOML.
pub fn candidate_config_paths(cfg: &Rclone) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(env) = std::env::var("RCLONE_CONFIG") {
        if !env.trim().is_empty() {
            paths.push(PathBuf::from(env.trim()));
        }
    }
    for p in &cfg.config_paths {
        paths.push(expand_tilde(p));
    }
    paths
}

/// Parse INI mínimo do rclone.conf → mapa `remote -> remote pai`.
/// Chaves case-insensitive (igual `configparser` do Python).
pub fn parse_remotes_mapping(text: &str) -> HashMap<String, String> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim().to_string();
            if !name.is_empty() {
                current = Some(name.clone());
                sections.entry(name).or_default();
            }
            continue;
        }
        let Some(section) = current.clone() else {
            continue;
        };
        let (k, v) = match line.split_once('=') {
            Some((k, v)) => (k, v),
            None => continue,
        };
        let key = k.trim().to_lowercase();
        let mut val = v.trim().to_string();
        // Remove comentário trailing ` # ...` / ` ; ...` fora de aspas.
        if let Some(idx) = val.find(" #") {
            val.truncate(idx);
        }
        if let Some(idx) = val.find(" ;") {
            val.truncate(idx);
        }
        let val = val.trim().trim_matches('"').trim_matches('\'').to_string();
        if key.is_empty() {
            continue;
        }
        if let Some(map) = sections.get_mut(&section) {
            map.insert(key, val);
        }
    }
    let mut mapping = HashMap::new();
    for (section, kv) in &sections {
        let mut parent = section.clone();
        if let Some(remote) = kv.get("remote") {
            let remote = remote.trim();
            if let Some((head, _)) = remote.split_once(':') {
                if !head.is_empty() && !head.starts_with('/') {
                    parent = head.to_string();
                }
            } else if !remote.is_empty() && !remote.starts_with('/') {
                parent = remote.to_string();
            }
        }
        mapping.insert(section.clone(), parent);
    }
    mapping
}

/// Lê o rclone.conf (primeiro existente) → mapa `remote -> pai`.
pub fn load_remotes_mapping(cfg: &Rclone) -> HashMap<String, String> {
    for p in candidate_config_paths(cfg) {
        if let Ok(text) = std::fs::read_to_string(&p) {
            return parse_remotes_mapping(&text);
        }
    }
    HashMap::new()
}

/// Nome básico do remote (`gdrive:media` → `gdrive`).
pub fn remote_name_from_device(device: &str) -> &str {
    match device.split_once(':') {
        Some((head, _)) => head,
        None => device,
    }
}

/// Remote pai via mapeamento do rclone.conf (fallback: o próprio nome).
pub fn parent_remote(device: &str, mapping: &HashMap<String, String>) -> String {
    let name = remote_name_from_device(device);
    mapping
        .get(name)
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

/// Info de mount remoto contendo `target`: (device, mountpoint, fstype, rel_path).
/// Escolhe o mount de prefixo mais longo (igual ao Python).
pub fn find_remote_mount_info(
    target: &Path,
    mounts: &[Mount],
    remote_markers: &[String],
) -> Option<(String, PathBuf, String, PathBuf)> {
    let target_abs = std::path::absolute(target).unwrap_or_else(|_| target.to_path_buf());
    let mut best: Option<(&Mount, usize)> = None;
    for m in mounts {
        if !is_remote_fs(&m.device, &m.fstype, remote_markers) {
            continue;
        }
        let mp = PathBuf::from(&m.mountpoint);
        let Ok(rel) = target_abs.strip_prefix(&mp) else {
            continue;
        };
        let len = m.mountpoint.len();
        if best.map(|(_, l)| len > l).unwrap_or(true) {
            best = Some((m, len));
        }
        let _ = rel;
    }
    best.map(|(m, _)| {
        let mp = PathBuf::from(&m.mountpoint);
        let rel = target_abs
            .strip_prefix(&mp)
            .map(Path::to_path_buf)
            .unwrap_or_default();
        (m.device.clone(), mp, m.fstype.clone(), rel)
    })
}

/// Caminho do cache VFS local p/ um path dentro do mount
/// (`cache_base/<remote>[/dev_path][/rel]`). Igual ao Python.
pub fn rclone_cache_path(
    target: &Path,
    device: &str,
    mountpoint: &Path,
    cache_base: &str,
) -> PathBuf {
    let base = expand_tilde(cache_base);
    let (remote_name, dev_path) = match device.split_once(':') {
        Some((r, d)) => (r, d.trim_start_matches('/')),
        None => (device, ""),
    };
    let rel = target
        .strip_prefix(mountpoint)
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut p = base.join(remote_name);
    if !dev_path.is_empty() {
        p = p.join(dev_path);
    }
    if !rel.as_os_str().is_empty() {
        p = p.join(rel);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
[gdrive]\ntype = drive\nscope = drive\n\
[crypt]\ntype = crypt\nremote = gdrive:/vault\n\
[alias]\ntype = alias\nremote = crypt:/media\n\
[local]\ntype = local\n";

    #[test]
    fn mapping_resolves_parents() {
        let m = parse_remotes_mapping(SAMPLE);
        assert_eq!(m.get("gdrive").unwrap(), "gdrive");
        assert_eq!(m.get("crypt").unwrap(), "gdrive");
        assert_eq!(m.get("alias").unwrap(), "crypt");
        assert_eq!(m.get("local").unwrap(), "local");
    }

    #[test]
    fn keys_are_case_insensitive() {
        let m = parse_remotes_mapping("[x]\nRemote = gdrive:/a\n");
        assert_eq!(m.get("x").unwrap(), "gdrive");
    }

    #[test]
    fn comments_and_quotes_ignored() {
        let m = parse_remotes_mapping("[x]\n# c\n; c\nremote = \"gdrive:/a\" # trailing\n");
        assert_eq!(m.get("x").unwrap(), "gdrive");
    }

    #[test]
    fn cache_path_matches_python_layout() {
        // Python: rel_from_mount inclui o dev_path (media/media duplicado
        // quando o mount é a raiz do remote). Port fiel.
        let target = PathBuf::from("/mnt/g/media/filmes/a.mkv");
        let mp = PathBuf::from("/mnt/g");
        let p = rclone_cache_path(&target, "gdrive:media", &mp, "~/.cache/rclone/vfs");
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            p,
            home.join(".cache/rclone/vfs/gdrive/media/media/filmes/a.mkv")
        );
    }

    #[test]
    fn cache_path_mount_root() {
        let mp = PathBuf::from("/mnt/g");
        let p = rclone_cache_path(&mp, "gdrive:", &mp, "/tmp/vfs");
        assert_eq!(p, PathBuf::from("/tmp/vfs/gdrive"));
    }

    #[test]
    fn find_remote_picks_longest_prefix() {
        let mounts = vec![
            Mount {
                device: "gdrive:".to_string(),
                mountpoint: "/mnt/g".to_string(),
                fstype: "fuse.rclone".to_string(),
            },
            Mount {
                device: "/dev/sda1".to_string(),
                mountpoint: "/".to_string(),
                fstype: "ext4".to_string(),
            },
        ];
        let markers = vec!["rclone".to_string()];
        let hit = find_remote_mount_info(Path::new("/mnt/g/a/b"), &mounts, &markers).unwrap();
        assert_eq!(hit.0, "gdrive:");
        assert_eq!(hit.3, PathBuf::from("a/b"));
        assert!(find_remote_mount_info(Path::new("/tmp/x"), &mounts, &markers).is_none());
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn mapping_never_panics(s in "\\PC{0,300}") {
            let _ = parse_remotes_mapping(&s);
        }

        #[test]
        fn remote_name_is_prefix_before_colon(s in "[a-zA-Z0-9_.-]{0,20}(:[a-z/]{0,20})?") {
            let name = remote_name_from_device(&s);
            prop_assert!(s.starts_with(name));
            prop_assert!(!name.contains(':'));
        }
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    #[test]
    fn tilde_and_env_expansion() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/x"), home.join("x"));
        std::env::set_var("DU_RS_TEST_EXPAND", "/tmp/exp");
        assert_eq!(
            expand_tilde("$DU_RS_TEST_EXPAND/sub"),
            PathBuf::from("/tmp/exp/sub")
        );
        assert_eq!(
            expand_tilde("${DU_RS_TEST_EXPAND}/a"),
            PathBuf::from("/tmp/exp/a")
        );
        assert_eq!(
            expand_tilde("/abs/$DU_RS_TEST_EXPAND"),
            PathBuf::from("/abs//tmp/exp")
        );
        assert_eq!(expand_tilde("plain"), PathBuf::from("plain"));
        std::env::remove_var("DU_RS_TEST_EXPAND");
    }

    #[test]
    fn candidate_paths_honor_env_first() {
        let tmp = std::env::temp_dir().join(format!("rclone-{}.conf", std::process::id()));
        std::fs::write(&tmp, "[solo]\ntype = drive\n").unwrap();
        std::env::set_var("RCLONE_CONFIG", &tmp);
        let cfg = Rclone::default();
        let paths = candidate_config_paths(&cfg);
        assert_eq!(paths.first().unwrap(), &tmp);
        // E o mapping carrega desse arquivo.
        let m = load_remotes_mapping(&cfg);
        assert_eq!(m.get("solo").unwrap(), "solo");
        std::env::remove_var("RCLONE_CONFIG");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn parent_remote_falls_back_to_name() {
        let empty = HashMap::new();
        assert_eq!(parent_remote("gdrive:media", &empty), "gdrive".to_string());
        assert_eq!(parent_remote("localdisk", &empty), "localdisk".to_string());
    }
}
