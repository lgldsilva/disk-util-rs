//! CLI (clap derive). Highest-precedence overrides over TOML/env.

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser, Clone)]
#[command(
    name = "disk-util-rs",
    version,
    about = "Interactive disk, RAM and system resource monitor (Rust)"
)]
pub struct Cli {
    /// Directory to analyze (default: `path` from config, else current directory).
    #[arg()]
    pub path: Option<PathBuf>,

    /// Show only volume/mountpoint usage.
    #[arg(long, env = "DISK_UTIL_ONLY_VOLUMES")]
    pub only_volumes: bool,

    /// Show only the subfolder analysis.
    #[arg(long, env = "DISK_UTIL_ONLY_FOLDERS")]
    pub only_folders: bool,

    /// For rclone mounts, show the local cache logical size instead of "—".
    #[arg(long, env = "DISK_UTIL_CLOUD_SIZE")]
    pub cloud_size: bool,

    /// Comma-separated list of SSH remote hosts. Use '' to disable.
    #[arg(long, env = "DISK_UTIL_SSH_HOSTS")]
    pub ssh_hosts: Option<String>,

    /// Interactive real-time monitoring mode (refresh every N seconds).
    #[arg(short, long, num_args = 0..=1, default_missing_value = "10", env = "DISK_UTIL_WATCH")]
    pub watch: Option<u64>,

    /// Run once (non-interactive) and exit (default when -w is not passed).
    #[arg(long, env = "DISK_UTIL_ONCE")]
    pub once: bool,

    /// Show boot partitions (/boot, /boot/efi), hidden by default.
    #[arg(long, env = "DISK_UTIL_SHOW_BOOT")]
    pub show_boot: bool,

    /// Alternative TOML config path.
    #[arg(long, env = "DISK_UTIL_CONFIG")]
    pub config: Option<PathBuf>,

    /// Generate the versioned `config.example.toml` and exit.
    #[arg(long)]
    pub generate_config_example: bool,
}

impl Cli {
    /// Effective directory: CLI > config `path` > current directory.
    pub fn effective_path(&self, cfg_path: &str) -> PathBuf {
        if let Some(ref p) = self.path {
            return p.clone();
        }
        if !cfg_path.is_empty() {
            return PathBuf::from(cfg_path);
        }
        PathBuf::from(".")
    }

    /// Effective SSH hosts: CLI > config. `Some(vec![])` disables.
    pub fn effective_ssh_hosts(&self, cfg_hosts: &[String]) -> Vec<String> {
        match &self.ssh_hosts {
            Some(s) => s
                .split(',')
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .map(str::to_string)
                .collect(),
            None => cfg_hosts.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("parse ok")
    }

    #[test]
    fn defaults_match_python() {
        let cli = parse(&["disk-util-rs"]);
        assert!(cli.path.is_none());
        assert!(!cli.only_volumes);
        assert!(!cli.only_folders);
        assert!(!cli.cloud_size);
        assert!(cli.ssh_hosts.is_none());
        assert!(cli.watch.is_none());
        assert!(!cli.once);
        assert!(!cli.show_boot);
    }

    #[test]
    fn effective_path_prefers_cli() {
        let cli = parse(&["disk-util-rs", "/tmp"]);
        assert_eq!(cli.effective_path("/cfg"), PathBuf::from("/tmp"));
        let cli = parse(&["disk-util-rs"]);
        assert_eq!(cli.effective_path("/cfg"), PathBuf::from("/cfg"));
        assert_eq!(cli.effective_path(""), PathBuf::from("."));
    }

    #[test]
    fn watch_bare_defaults_to_10() {
        let cli = parse(&["disk-util-rs", "--watch"]);
        assert_eq!(cli.watch, Some(10));
        let cli = parse(&["disk-util-rs", "-w", "5"]);
        assert_eq!(cli.watch, Some(5));
    }

    #[test]
    fn ssh_hosts_empty_disables() {
        let cli = parse(&["disk-util-rs", "--ssh-hosts", ""]);
        assert!(cli.effective_ssh_hosts(&["a".to_string()]).is_empty());
    }

    #[test]
    fn ssh_hosts_split_and_trim() {
        let cli = parse(&["disk-util-rs", "--ssh-hosts", "arm-node1, arm-node2,,"]);
        assert_eq!(cli.effective_ssh_hosts(&[]), vec!["arm-node1", "arm-node2"]);
    }

    #[test]
    fn ssh_hosts_falls_back_to_config() {
        let cli = parse(&["disk-util-rs"]);
        assert_eq!(
            cli.effective_ssh_hosts(&["h1".to_string()]),
            vec!["h1".to_string()]
        );
    }
}
