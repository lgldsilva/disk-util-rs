//! Configuração dinâmica via TOML (figment + serde `deny_unknown_fields`).
//!
//! Hierarquia (último vence): [`AppConfig::default`] < arquivo
//! `~/.config/disk-util/config.toml` < env `DISK_UTIL_*` (nested com `__`)
//! < CLI (`--watch/--ssh-hosts/...`).
//! Hot reload: tecla `R` na TUI + `SIGHUP` (ver `main.rs`).
//!
//! Mesmo schema style de `docker-top-rs` p/ futuro `cfgkit`:
//! seções nomeadas, defaults no código, `deny_unknown_fields` em tudo.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};

use crate::cli::Cli;

/// Caminho padrão do arquivo de configuração.
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("disk-util")
        .join("config.toml")
}

/// Caminho do `config.example.toml` versionado (gerado por [`generate_example_file`]).
pub fn example_config_path() -> PathBuf {
    PathBuf::from("config.example.toml")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Timeouts {
    /// Walk nativo de um subitem (s). Timeout vira status `timeout`.
    pub walk_item_secs: f64,
    /// Walk do cache rclone (s). FUSE nunca é walkado.
    pub walk_cache_secs: f64,
    /// `ssh host df -Pk` por host (s).
    pub ssh_secs: f64,
    /// TTL do cache SSH por host (s).
    pub ssh_cache_secs: f64,
    /// Refresh da lista de discos sysinfo (s). Em timeout reusa anterior.
    pub sysinfo_secs: f64,
    /// Timeout do `lsblk`/`zpool` da descoberta de discos (s).
    pub disk_discover_secs: f64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            walk_item_secs: 3.0,
            walk_cache_secs: 2.0,
            ssh_secs: 4.0,
            ssh_cache_secs: 30.0,
            sysinfo_secs: 10.0,
            disk_discover_secs: 2.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Thresholds {
    /// RAM amarela a partir de (%).
    pub ram_warn: f64,
    /// RAM vermelha a partir de (%).
    pub ram_crit: f64,
    /// Disco amarelo a partir de (%).
    pub disk_warn: f64,
    /// Disco vermelho a partir de (%).
    pub disk_crit: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            ram_warn: 70.0,
            ram_crit: 85.0,
            disk_warn: 75.0,
            disk_crit: 90.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Rclone {
    /// Caminhos do rclone.conf (`~` expande; `$RCLONE_CONFIG` tem prioridade).
    pub config_paths: Vec<String>,
    /// Base do cache VFS (`~` expande).
    pub cache_base: String,
    /// Backend sem cota real a partir deste total (bytes). Padrão 1 PiB.
    pub fake_size_threshold: u64,
}

impl Default for Rclone {
    fn default() -> Self {
        Self {
            config_paths: vec![
                "~/.config/rclone/rclone.conf".to_string(),
                "~/.rclone.conf".to_string(),
            ],
            cache_base: "~/.cache/rclone/vfs".to_string(),
            fake_size_threshold: 1 << 50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Ssh {
    /// StrictHostKeyChecking: yes|no|accept-new|off (padrão accept-new, não `no`).
    pub strict_host_key_checking: String,
    /// ConnectTimeout ssh (s).
    pub connect_timeout_secs: f64,
    /// Paralelismo máximo ssh.
    pub max_parallel: usize,
    /// Descobre hosts dinamicamente no `~/.ssh/config` (mescla com `ssh_hosts`).
    pub auto_discover: bool,
    /// Hosts descobertos/config a excluir da varredura.
    pub ignore_hosts: Vec<String>,
}

impl Default for Ssh {
    fn default() -> Self {
        Self {
            strict_host_key_checking: "accept-new".to_string(),
            connect_timeout_secs: 2.0,
            max_parallel: 4,
            auto_discover: true,
            ignore_hosts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Partitions {
    /// Substrings de device que marcam partição local relevante.
    pub device_match: Vec<String>,
    /// Mountpoints locais sempre relevantes.
    pub mountpoints: Vec<String>,
    /// Prefixos de fstype locais (ex: nfs).
    pub nfs_prefixes: Vec<String>,
    /// Substrings de fstype/device que marcam mount remoto (rclone/FUSE nuvem).
    pub remote_markers: Vec<String>,
    /// Consolidação de volumes locais: `disk` (uma linha por disco físico/
    /// pool ZFS, via lsblk+zpool) ou `device` (uma linha por device, legado).
    pub group_by: String,
}

impl Default for Partitions {
    fn default() -> Self {
        Self {
            device_match: vec![
                "/dev/sd",
                "/dev/nvme",
                "/dev/mapper",
                "lpool",
                "rpool",
                "disk",
                "volume",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            mountpoints: vec!["/".to_string(), "/storage".to_string()],
            nfs_prefixes: vec!["nfs".to_string()],
            remote_markers: vec!["rclone".to_string()],
            group_by: "disk".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Boot {
    /// Prefixos de mount ocultos por padrão (`--show-boot`/`b` revela).
    pub paths: Vec<String>,
}

impl Default for Boot {
    fn default() -> Self {
        Self {
            paths: vec!["/boot".to_string()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Display {
    /// Formato de bytes: `binary` (GiB/MiB) ou `decimal` (GB/MB).
    pub byte_format: String,
    /// Casas decimais nos formatos humanos.
    pub precision: usize,
    /// Formato do relógio no header.
    pub clock_format: String,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            byte_format: "binary".to_string(),
            precision: 2,
            clock_format: "%H:%M:%S".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Keybindings {
    pub quit: String,
    pub filter: String,
    pub search: String,
    pub sort_cycle: String,
    pub toggle_order: String,
    pub reload_config: String,
    pub rescan: String,
    pub show_boot: String,
    pub help: String,
    pub back: String,
    pub enter: String,
    /// Popup com detalhes das falhas de conexão SSH.
    pub ssh_errors: String,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            quit: "q".to_string(),
            filter: "f".to_string(),
            search: "/".to_string(),
            sort_cycle: "o".to_string(),
            toggle_order: "s".to_string(),
            reload_config: "R".to_string(),
            rescan: "r".to_string(),
            show_boot: "b".to_string(),
            help: "h".to_string(),
            back: "backspace".to_string(),
            enter: "enter".to_string(),
            ssh_errors: "d".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Layout {
    /// Linhas reservadas (header + footer) ao paginar o navegador.
    pub reserved_rows: u16,
    /// Largura mínima p/ colunas completas do navegador.
    pub wide_width: u16,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            reserved_rows: 8,
            wide_width: 160,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AppConfig {
    /// Intervalo de coleta no modo watch (s). Clamp aplicado em `load`.
    pub refresh_delay: f64,
    /// Diretório inicial quando a CLI não passa `path`.
    pub path: String,
    /// Hosts SSH extras mesclados com a descoberta do `~/.ssh/config`
    /// (CLI `--ssh-hosts` vence com lista exata; `''` desativa).
    pub ssh_hosts: Vec<String>,
    /// Mostra partições de boot por padrão.
    pub show_boot: bool,
    /// Ordenação inicial do navegador (`physical|logical|name|type`).
    pub sort_by: String,
    /// Inicia ascendente quando true.
    pub ascending: bool,
    /// Filtro inicial de nome.
    pub filter: String,
    pub timeouts: Timeouts,
    pub thresholds: Thresholds,
    pub rclone: Rclone,
    pub partitions: Partitions,
    pub boot: Boot,
    pub display: Display,
    pub keybindings: Keybindings,
    pub layout: Layout,
    pub ssh: Ssh,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            refresh_delay: 10.0,
            path: ".".to_string(),
            ssh_hosts: Vec::new(),
            show_boot: false,
            sort_by: "physical".to_string(),
            ascending: false,
            filter: String::new(),
            timeouts: Timeouts::default(),
            thresholds: Thresholds::default(),
            rclone: Rclone::default(),
            partitions: Partitions::default(),
            boot: Boot::default(),
            display: Display::default(),
            keybindings: Keybindings::default(),
            layout: Layout::default(),
            ssh: Ssh::default(),
        }
    }
}

const SORT_COLUMNS: [&str; 4] = ["physical", "logical", "name", "type"];
const GROUP_BY_MODES: [&str; 2] = ["disk", "device"];

impl AppConfig {
    /// Monta figment com a hierarquia default < arquivo < env < CLI.
    pub fn figment(cli: &Cli, config_path: &Path) -> Figment {
        let mut cli_map: HashMap<String, figment::value::Value> = HashMap::new();
        if let Some(w) = cli.watch {
            cli_map.insert(
                "refresh_delay".to_string(),
                figment::value::Value::from(w as f64),
            );
        }
        if let Some(ref s) = cli.ssh_hosts {
            let hosts: Vec<String> = s
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string)
                .collect();
            cli_map.insert("ssh_hosts".to_string(), figment::value::Value::from(hosts));
        }
        if cli.show_boot {
            cli_map.insert("show_boot".to_string(), figment::value::Value::from(true));
        }
        Figment::from(Serialized::defaults(AppConfig::default()))
            .merge(Toml::file(config_path))
            .merge(Env::prefixed("DISK_UTIL_").split("__"))
            .merge(Serialized::defaults(cli_map))
    }

    /// Carrega e valida (clamp + sort conhecido).
    pub fn load(cli: &Cli, config_path: &Path) -> anyhow::Result<Self> {
        let mut cfg: AppConfig = Self::figment(cli, config_path).extract().map_err(|e| {
            anyhow::anyhow!("config inválida (deny_unknown_fields, ver config.example.toml): {e}")
        })?;
        cfg.refresh_delay = cfg.refresh_delay.clamp(1.0, 300.0);
        cfg.timeouts.walk_item_secs = cfg.timeouts.walk_item_secs.clamp(0.5, 60.0);
        cfg.timeouts.walk_cache_secs = cfg.timeouts.walk_cache_secs.clamp(0.5, 60.0);
        cfg.timeouts.ssh_secs = cfg.timeouts.ssh_secs.clamp(1.0, 60.0);
        cfg.timeouts.ssh_cache_secs = cfg.timeouts.ssh_cache_secs.clamp(5.0, 3600.0);
        cfg.timeouts.disk_discover_secs = cfg.timeouts.disk_discover_secs.clamp(0.5, 30.0);
        if !SORT_COLUMNS.contains(&cfg.sort_by.as_str()) {
            anyhow::bail!(
                "sort_by inicial desconhecido '{}'. Válidos: {:?}",
                cfg.sort_by,
                SORT_COLUMNS
            );
        }
        if !GROUP_BY_MODES.contains(&cfg.partitions.group_by.as_str()) {
            anyhow::bail!(
                "partitions.group_by desconhecido '{}'. Válidos: {:?}",
                cfg.partitions.group_by,
                GROUP_BY_MODES
            );
        }
        Ok(cfg)
    }

    /// Hot reload: relê arquivo+env mantendo overrides da CLI.
    pub fn reload(&mut self, cli: &Cli, config_path: &Path) -> anyhow::Result<()> {
        let fresh = Self::load(cli, config_path)?;
        *self = fresh;
        Ok(())
    }
}

/// Gera o `config.example.toml` versionado a partir do default serializado.
pub fn generate_example_file(path: &Path) -> anyhow::Result<()> {
    let cfg = AppConfig::default();
    let mut doc = String::from(
        "# disk-util-rs — exemplo de configuração (copie p/ ~/.config/disk-util/config.toml)\n\
         # Hierarquia: default < arquivo < env DISK_UTIL_* (nested com __) < CLI\n\
         # Hot reload: tecla R ou SIGHUP. Campos desconhecidos = erro (deny_unknown_fields).\n\n",
    );
    doc.push_str(&toml::to_string_pretty(&cfg)?);
    doc.push_str(
        "\n# Env exemplos:\n\
         #   DISK_UTIL_REFRESH_DELAY=10\n\
         #   DISK_UTIL_SSH_HOSTS=arm-node1,arm-node2\n\
         #   DISK_UTIL_TIMEOUTS__SSH_SECS=4\n\
         #   DISK_UTIL_RCLONE__CACHE_BASE=~/.cache/rclone/vfs\n\
         # CLI: --watch 10 --ssh-hosts arm-node1 --show-boot --once --only-volumes\n",
    );
    std::fs::write(path, doc)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;

    fn test_cli() -> Cli {
        Cli::try_parse_from(["disk-util-rs"]).expect("parse ok")
    }

    #[test]
    fn default_load_without_file_ok() {
        let cli = test_cli();
        let cfg = AppConfig::load(&cli, &PathBuf::from("/nonexistent/disk-util.toml")).unwrap();
        assert!((cfg.refresh_delay - 10.0).abs() < f64::EPSILON);
        assert_eq!(cfg.sort_by, "physical");
        assert!((cfg.timeouts.ssh_secs - 4.0).abs() < f64::EPSILON);
        assert!((cfg.timeouts.ssh_cache_secs - 30.0).abs() < f64::EPSILON);
        assert_eq!(cfg.rclone.fake_size_threshold, 1 << 50);
        assert_eq!(cfg.boot.paths, vec!["/boot".to_string()]);
    }

    #[test]
    fn cli_overrides_win() {
        let cli = Cli::try_parse_from([
            "disk-util-rs",
            "--ssh-hosts",
            "h1,h2",
            "--watch",
            "20",
            "--show-boot",
        ])
        .expect("parse");
        let cfg = AppConfig::load(&cli, &PathBuf::from("/nonexistent/x.toml")).unwrap();
        assert_eq!(cfg.ssh_hosts, vec!["h1".to_string(), "h2".to_string()]);
        assert_eq!(cfg.refresh_delay, 20.0);
        assert!(cfg.show_boot);
    }

    #[test]
    fn unknown_field_rejected() {
        let dir = std::env::temp_dir().join(format!("du-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.toml");
        std::fs::write(&p, "refresh_delay = 10.0\ncampo_inexistente = 1\n").unwrap();
        let cli = test_cli();
        assert!(AppConfig::load(&cli, &p).is_err());
    }

    #[test]
    fn toml_file_overrides_default() {
        let dir = std::env::temp_dir().join(format!("du2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ok.toml");
        std::fs::write(&p, "[thresholds]\nram_warn = 60.0\ndisk_crit = 95.0\n").unwrap();
        let cli = test_cli();
        let cfg = AppConfig::load(&cli, &p).unwrap();
        assert!((cfg.thresholds.ram_warn - 60.0).abs() < f64::EPSILON);
        assert!((cfg.thresholds.disk_crit - 95.0).abs() < f64::EPSILON);
    }

    #[test]
    fn invalid_sort_rejected() {
        let dir = std::env::temp_dir().join(format!("du3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("sort.toml");
        std::fs::write(&p, "sort_by = \"banana\"\n").unwrap();
        let cli = test_cli();
        assert!(AppConfig::load(&cli, &p).is_err());
    }

    #[test]
    fn ssh_discovery_and_group_by_defaults() {
        let cli = test_cli();
        let cfg = AppConfig::load(&cli, &PathBuf::from("/nonexistent/x.toml")).unwrap();
        assert!(cfg.ssh.auto_discover);
        assert!(cfg.ssh.ignore_hosts.is_empty());
        assert!(cfg.ssh_hosts.is_empty(), "sem hardcode de hosts no default");
        assert_eq!(cfg.partitions.group_by, "disk");
        assert!((cfg.timeouts.disk_discover_secs - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn invalid_group_by_rejected() {
        let dir = std::env::temp_dir().join(format!("du4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("gb.toml");
        std::fs::write(&p, "[partitions]\ngroup_by = \"banana\"\n").unwrap();
        let cli = test_cli();
        assert!(AppConfig::load(&cli, &p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;

    fn test_cli() -> Cli {
        Cli::try_parse_from(["disk-util-rs"]).expect("parse")
    }

    #[test]
    fn paths_point_to_disk_util() {
        assert!(default_config_path().ends_with("disk-util/config.toml"));
        assert_eq!(example_config_path(), PathBuf::from("config.example.toml"));
    }

    #[test]
    fn generate_then_reload_roundtrip() {
        let dir = std::env::temp_dir().join(format!("du-ex-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.example.toml");
        generate_example_file(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("refresh_delay"));
        assert!(text.contains("DISK_UTIL_"));
        let cli = test_cli();
        let cfg = AppConfig::load(&cli, &p).unwrap();
        assert!((cfg.refresh_delay - 10.0).abs() < f64::EPSILON);
        // Reload aplica edição do arquivo.
        std::fs::write(&p, "refresh_delay = 7.5\n").unwrap();
        let mut cfg2 = cfg;
        cfg2.reload(&cli, &p).unwrap();
        assert!((cfg2.refresh_delay - 7.5).abs() < f64::EPSILON);
        // Reload com erro preserva anterior.
        std::fs::write(&p, "campo_inexistente = 1\n").unwrap();
        assert!(cfg2.reload(&cli, &p).is_err());
        assert!((cfg2.refresh_delay - 7.5).abs() < f64::EPSILON);
    }
}
