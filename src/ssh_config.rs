//! Descoberta dinâmica de hosts SSH a partir de `~/.ssh/config`.
//!
//! - aliases de blocos `Host` (fora de `Match`), sem padrões (`*`, `?`, `!`);
//! - `Include` resolvido com glob simples (`*`/`?` no nome do arquivo),
//!   relativo a `~/.ssh`, profundidade limitada;
//! - dedupe de aliases que apontam para o mesmo alvo (HostName+Port):
//!   `arm-node1`/`arm-node2`/`arm-node3` colapsam no primeiro alias;
//! - merge puro em [`resolve_hosts`]: CLI (lista exata; `''` desativa) >
//!   config + descoberta − `ignore_hosts`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Caminho padrão do `~/.ssh/config`.
pub fn default_ssh_config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".ssh")
        .join("config")
}

/// Um bloco `Host` do ssh_config (blocos `Match` não viram blocos).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshBlock {
    pub aliases: Vec<String>,
    pub hostname: Option<String>,
    pub port: Option<String>,
}

/// Parse puro de um ssh_config já carregado em texto.
/// Blocos `Match` são ignorados (HostName condicional não serve de alvo).
pub fn parse_ssh_config(text: &str) -> Vec<SshBlock> {
    parse_into(text, None, 0)
}

/// Parse com resolução de `Include` (recursivo, profundidade limitada).
/// `base_dir` resolve includes relativos (padrão ssh: `~/.ssh`).
fn parse_into(text: &str, base_dir: Option<&Path>, depth: usize) -> Vec<SshBlock> {
    let mut blocks: Vec<SshBlock> = Vec::new();
    if depth > 3 {
        return blocks;
    }
    let mut current: Option<SshBlock> = None;
    let mut in_match = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (kw, rest) = match line.split_once(char::is_whitespace) {
            Some((k, r)) => (k.to_ascii_lowercase(), r.trim()),
            None => (line.to_ascii_lowercase(), ""),
        };
        match kw.as_str() {
            "host" => {
                in_match = false;
                if let Some(b) = current.take() {
                    blocks.push(b);
                }
                let aliases = rest
                    .split_whitespace()
                    .filter(|a| !a.starts_with('#'))
                    .filter(|a| !a.contains('*') && !a.contains('?') && !a.starts_with('!'))
                    .map(str::to_string)
                    .collect();
                current = Some(SshBlock {
                    aliases,
                    hostname: None,
                    port: None,
                });
            }
            "match" => {
                if let Some(b) = current.take() {
                    blocks.push(b);
                }
                in_match = true;
            }
            "hostname" if !in_match => {
                if let Some(b) = current.as_mut() {
                    b.hostname = Some(rest.to_string());
                }
            }
            "port" if !in_match => {
                if let Some(b) = current.as_mut() {
                    b.port = Some(rest.to_string());
                }
            }
            "include" if !in_match => {
                if let Some(b) = current.take() {
                    blocks.push(b);
                }
                if let Some(dir) = base_dir {
                    for arg in rest.split_whitespace() {
                        for path in resolve_include(arg, dir) {
                            if let Ok(text) = std::fs::read_to_string(&path) {
                                blocks.extend(parse_into(
                                    &text,
                                    Some(path.parent().unwrap_or(dir)),
                                    depth + 1,
                                ));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(b) = current.take() {
        blocks.push(b);
    }
    blocks
}

/// Expande `~`, resolve relativo a `base_dir` e aplica glob `*`/`?`
/// (apenas no nome do arquivo; diretórios com wildcard são ignorados).
fn resolve_include(arg: &str, base_dir: &Path) -> Vec<PathBuf> {
    let expanded = if let Some(rest) = arg.strip_prefix("~/") {
        match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => return Vec::new(),
        }
    } else {
        PathBuf::from(arg)
    };
    let path = if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    };
    let text = path.to_string_lossy().into_owned();
    if !text.contains('*') && !text.contains('?') {
        return vec![path];
    }
    let file_pattern = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| glob_match(&file_pattern, &e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// Matcher glob mínimo (`*` = qualquer sequência, `?` = 1 caractere).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_rec(&p, &t)
}

fn glob_rec(p: &[char], t: &[char]) -> bool {
    match p.split_first() {
        None => t.is_empty(),
        Some(('*', rest)) => (0..=t.len()).any(|i| glob_rec(rest, &t[i..])),
        Some((pc, rest)) => match t.split_first() {
            Some((tc, trest)) if *pc == '?' || pc == tc => glob_rec(rest, trest),
            _ => false,
        },
    }
}

/// Aliases únicos na ordem do arquivo (wildcards/negados fora).
pub fn aliases_from_blocks(blocks: &[SshBlock]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for b in blocks {
        for a in &b.aliases {
            if seen.insert(a.clone()) {
                out.push(a.clone());
            }
        }
    }
    out
}

/// Descobre alvos únicos: dedupe por HostName+Port, mantendo o 1º alias.
/// Arquivo ausente/ilegível → lista vazia (nunca falha).
pub fn discover_hosts(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let blocks = parse_into(&text, Some(path.parent().unwrap_or(path)), 0);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut out = Vec::new();
    for b in blocks {
        let Some(first) = b.aliases.first() else {
            continue;
        };
        let target = (
            b.hostname.clone().unwrap_or_else(|| first.clone()),
            b.port.clone().unwrap_or_else(|| "22".to_string()),
        );
        if seen.insert(target) {
            out.push(first.clone());
        }
    }
    out
}

/// Resolve a lista efetiva de hosts SSH:
/// - CLI `Some(csv)`: lista exata (string vazia desativa o SSH);
/// - CLI `None`: config + descoberta (quando `auto_discover`) −
///   `ignore_hosts`, dedupe e ordenado (determinismo).
pub fn resolve_hosts(
    cli_hosts: Option<&str>,
    cfg_hosts: &[String],
    auto_discover: bool,
    ignore_hosts: &[String],
    discover: &dyn Fn() -> Vec<String>,
) -> Vec<String> {
    if let Some(csv) = cli_hosts {
        return csv
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_string)
            .collect();
    }
    let mut out: Vec<String> = cfg_hosts.to_vec();
    if auto_discover {
        for h in discover() {
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out.retain(|h| !ignore_hosts.iter().any(|i| i == h));
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = "\
# comentário
Host pi-server lab-node1
	HostName 192.0.2.10
	User admin
	Port 22

Match originalhost laptop !exec \"cmd\"
	HostName 198.51.100.7

Host laptop
	HostName 192.0.2.20

Host arm-node1
	HostName 203.0.113.5

Host arm-node2 arm-node3 arm-node4
	HostName 203.0.113.5

Host *
	IdentitiesOnly yes

Host *-wg
	HostName 192.0.2.30
";

    #[test]
    fn parses_aliases_skips_wildcards_and_match() {
        let blocks = parse_ssh_config(CFG);
        let aliases = aliases_from_blocks(&blocks);
        assert_eq!(
            aliases,
            vec![
                "pi-server",
                "lab-node1",
                "laptop",
                "arm-node1",
                "arm-node2",
                "arm-node3",
                "arm-node4",
            ]
        );
        assert!(!aliases.contains(&"*".to_string()));
        assert!(!aliases.contains(&"*-wg".to_string()));
        // Bloco Match não vira bloco; HostName do Match não vaza p/ bloco anterior.
        let mac = blocks
            .iter()
            .find(|b| b.aliases.contains(&"laptop".to_string()));
        assert_eq!(mac.and_then(|b| b.hostname.as_deref()), Some("192.0.2.20"));
    }

    #[test]
    fn discover_dedupes_by_hostname_and_port() {
        let dir = std::env::temp_dir().join(format!("du-sshcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config");
        std::fs::write(&path, CFG).unwrap();
        let hosts = discover_hosts(&path);
        assert_eq!(
            hosts,
            vec!["pi-server", "laptop", "arm-node1"],
            "aliases do mesmo IP colapsam no 1º"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_handles_port_and_missing_file() {
        let dir = std::env::temp_dir().join(format!("du-sshcfg2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config");
        std::fs::write(
            &path,
            "Host a\n  HostName 1.2.3.4\n  Port 2222\nHost b\n  HostName 1.2.3.4\n",
        )
        .unwrap();
        assert_eq!(
            discover_hosts(&path),
            vec!["a", "b"],
            "porta diferente = alvo diferente"
        );
        let missing = discover_hosts(&dir.join("nao-existe"));
        assert!(missing.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn include_resolves_relative_and_glob() {
        let dir = std::env::temp_dir().join(format!("du-sshcfg3-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("config.d")).unwrap();
        std::fs::write(dir.join("config.d/a.conf"), "Host included-a\n").unwrap();
        std::fs::write(dir.join("config.d/b.conf"), "Host included-b\n").unwrap();
        std::fs::write(dir.join("config"), "Host main\nInclude config.d/*.conf\n").unwrap();
        let blocks = parse_into(
            &std::fs::read_to_string(dir.join("config")).unwrap(),
            Some(&dir),
            0,
        );
        let aliases = aliases_from_blocks(&blocks);
        assert!(aliases.contains(&"included-a".to_string()), "{aliases:?}");
        assert!(aliases.contains(&"included-b".to_string()), "{aliases:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("*.conf", "a.conf"));
        assert!(glob_match("*.conf", "x.y.conf"));
        assert!(!glob_match("*.conf", "a.txt"));
        assert!(glob_match("config-?.conf", "config-1.conf"));
        assert!(!glob_match("config-?.conf", "config-12.conf"));
        assert!(glob_match("plain", "plain"));
    }

    #[test]
    fn resolve_cli_wins_and_empty_disables() {
        let discover = || vec!["d1".to_string()];
        assert!(
            resolve_hosts(Some(""), &["x".to_string()], true, &[], &discover).is_empty(),
            "'' desativa o SSH"
        );
        assert_eq!(
            resolve_hosts(Some(" a, ,b "), &["x".to_string()], true, &[], &discover),
            vec!["a", "b"]
        );
    }

    #[test]
    fn resolve_merges_config_with_discovery_and_ignores() {
        let discover = || {
            vec![
                "arm-node1".to_string(),
                "gitea".to_string(),
                "arm-node1".to_string(),
            ]
        };
        let out = resolve_hosts(
            None,
            &["arm-node1".to_string()],
            true,
            &["gitea".to_string()],
            &discover,
        );
        assert_eq!(out, vec!["arm-node1"], "dedupe + ignore_hosts");
        let out = resolve_hosts(None, &["z".to_string()], false, &[], &discover);
        assert_eq!(out, vec!["z"], "auto_discover=false usa só a config");
        let out = resolve_hosts(None, &[], true, &[], &|| Vec::new());
        assert!(out.is_empty(), "sem config e sem descoberta = vazio");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn parser_never_panics(s in "\\PC{0,300}") {
            let _ = parse_ssh_config(&s);
            let _ = aliases_from_blocks(&parse_ssh_config(&s));
        }

        #[test]
        fn glob_never_panics(p in "[a-zA-Z0-9.*?_/-]{0,20}", t in "[a-zA-Z0-9._/-]{0,20}") {
            let _ = glob_match(&p, &t);
        }
    }
}
