//! SSH remoto: fork `ssh host df -Pk` (NÃO russh).
//!
//! - `tokio::process` com argv array (sem shell);
//! - preserva `~/.ssh/config` (ProxyJump etc): sem `-F`, só BatchMode/ConnectTimeout;
//! - concorrência limitada por [`tokio::sync::Semaphore`];
//! - timeout por host + cache configurável (padrão 30s).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::sync::Semaphore;

/// Partição remota parseada do `df -Pk`.
#[derive(Debug, Clone, PartialEq)]
pub struct RemotePartition {
    pub device: String,
    pub fstype: String,
    pub size: u64,
    pub used: u64,
    pub free: u64,
    pub used_pct: f64,
    pub mountpoint: String,
    pub is_remote: bool,
}

/// Parse do `df -Pk` (POSIX: blocos de 1024, 1 FS por linha).
/// Localiza o token `NN%` p/ tolerar device/mount com espaços.
pub fn parse_df_posix(
    stdout: &str,
    is_interesting: &impl Fn(&str, &str, &str) -> (bool, bool),
) -> Vec<RemotePartition> {
    let mut out = Vec::new();
    for line in stdout.lines().skip(1) {
        if let Some(p) = parse_df_line(line) {
            let (remote, local) = is_interesting(&p.device, &p.fstype, &p.mountpoint);
            if remote || local {
                out.push(RemotePartition {
                    device: p.device,
                    fstype: p.fstype,
                    size: p.size,
                    used: p.used,
                    free: p.free,
                    used_pct: p.used_pct,
                    mountpoint: p.mountpoint,
                    is_remote: remote,
                });
            }
        }
    }
    out
}

struct DfRow {
    device: String,
    fstype: String,
    size: u64,
    used: u64,
    free: u64,
    used_pct: f64,
    mountpoint: String,
}

/// `df -Pk` não tem coluna fstype; marcamos `""` e o filtro decide.
/// (No host remoto o Python usava `df --output=source,fstype,...`; aqui
/// usamos `-Pk` portátil e classificamos por device/mount.)
fn parse_df_line(line: &str) -> Option<DfRow> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let pct_idx = toks.iter().position(|t| t.ends_with('%'))?;
    if pct_idx < 4 || pct_idx + 1 >= toks.len() {
        return None;
    }
    // Campos numéricos são os 3 tokens imediatamente antes do `NN%`.
    let size = toks[pct_idx - 3].parse::<u64>().ok()?;
    let used = toks[pct_idx - 2].parse::<u64>().ok()?;
    let free = toks[pct_idx - 1].parse::<u64>().ok()?;
    let used_pct = toks[pct_idx].trim_end_matches('%').parse::<f64>().ok()?;
    let device = toks[..pct_idx - 3].join(" ");
    if device.is_empty() {
        return None;
    }
    let mountpoint = toks[pct_idx + 1..].join(" ");
    if mountpoint.is_empty() {
        return None;
    }
    Some(DfRow {
        device,
        fstype: String::new(),
        size: size * 1024,
        used: used * 1024,
        free: free * 1024,
        used_pct,
        mountpoint,
    })
}

type CacheMap = DashMap<String, (Instant, Vec<RemotePartition>)>;

/// Tipo de falha de conexão SSH.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshFailureKind {
    /// Sem resposta dentro do timeout configurado.
    Timeout,
    /// Binário `ssh` não pôde ser executado.
    Spawn,
    /// `ssh` executou e saiu com erro (exit != 0).
    NonZero,
}

/// Falha de conexão a um host (nunca vai ao cache TTL: re-tenta no próximo tick).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshFailure {
    pub kind: SshFailureKind,
    pub message: String,
}

impl SshFailure {
    /// Mensagem única para UI/relatório: `timeout: …`, `spawn: …`, `exit: …`.
    pub fn describe(&self) -> String {
        let kind = match self.kind {
            SshFailureKind::Timeout => "timeout",
            SshFailureKind::Spawn => "spawn",
            SshFailureKind::NonZero => "exit",
        };
        format!("{kind}: {}", self.message)
    }
}

/// Achata stderr em uma linha e trunca (`…` no corte).
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let cut: String = flat.chars().take(max).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

/// Resultado de [`SshClient::fetch_all`]: partições por host ok + falha por host.
#[derive(Debug, Clone, Default)]
pub struct SshFetch {
    pub partitions: HashMap<String, Vec<RemotePartition>>,
    pub errors: HashMap<String, String>,
}

/// Cliente SSH com cache TTL e paralelismo limitado.
#[derive(Debug, Clone)]
pub struct SshClient {
    semaphore: Arc<Semaphore>,
    pub timeout: Duration,
    pub cache_ttl: Duration,
    strict_host_key_checking: String,
    connect_timeout_secs: f64,
    cache: Arc<CacheMap>,
}

impl SshClient {
    pub fn new(max_parallel: usize, timeout: Duration, cache_ttl: Duration) -> Self {
        Self::new_full(
            max_parallel,
            timeout,
            cache_ttl,
            "accept-new".to_string(),
            2.0,
        )
    }

    pub fn new_full(
        max_parallel: usize,
        timeout: Duration,
        cache_ttl: Duration,
        strict_host_key_checking: String,
        connect_timeout_secs: f64,
    ) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_parallel.max(1))),
            timeout,
            cache_ttl,
            strict_host_key_checking,
            connect_timeout_secs,
            cache: Arc::new(DashMap::new()),
        }
    }

    /// Roda `ssh host df -Pk` com timeout. argv array, sem shell.
    /// Falha vira [`SshFailure`] (não cacheada — host que volta fica ok no
    /// tick seguinte).
    pub async fn fetch_host(&self, host: &str) -> Result<String, SshFailure> {
        let _permit = self.semaphore.acquire().await.map_err(|_| SshFailure {
            kind: SshFailureKind::Spawn,
            message: "semáforo fechado".to_string(),
        })?;
        let connect_timeout = format!("ConnectTimeout={}", self.connect_timeout_secs as u64);
        let run = tokio::process::Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                &format!("StrictHostKeyChecking={}", self.strict_host_key_checking),
                "-o",
                &connect_timeout,
                host,
                "df -Pk",
            ])
            .output();
        let out = match tokio::time::timeout(self.timeout, run).await {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                return Err(SshFailure {
                    kind: SshFailureKind::Spawn,
                    message: format!("falha ao executar ssh: {e}"),
                });
            }
            Err(_) => {
                return Err(SshFailure {
                    kind: SshFailureKind::Timeout,
                    message: format!("sem resposta após {:.0}s", self.timeout.as_secs_f64()),
                });
            }
        };
        if !out.status.success() {
            let code = out
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "?".to_string());
            return Err(SshFailure {
                kind: SshFailureKind::NonZero,
                message: format!(
                    "ssh exit {code}: {}",
                    one_line(&String::from_utf8_lossy(&out.stderr), 160)
                ),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Todos os hosts em paralelo; usa cache TTL quando válido.
    /// Sucessos vão a `partitions`; falhas a `errors` (host → motivo).
    pub async fn fetch_all<F>(&self, hosts: &[String], is_interesting: &F) -> SshFetch
    where
        F: Fn(&str, &str, &str) -> (bool, bool) + Send + Sync,
    {
        let mut out = SshFetch::default();
        if hosts.is_empty() {
            return out;
        }
        let mut set = tokio::task::JoinSet::new();
        for host in hosts {
            if let Some(entry) = self.cache.get(host) {
                if entry.0.elapsed() < self.cache_ttl {
                    out.partitions.insert(host.clone(), entry.1.clone());
                    continue;
                }
            }
            let client = self.clone();
            let host = host.clone();
            set.spawn(async move { (host.clone(), client.fetch_host(&host).await) });
        }
        while let Some(res) = set.join_next().await {
            let Ok((host, result)) = res else { continue };
            match result {
                Ok(text) => {
                    let parsed = parse_df_posix(&text, is_interesting);
                    self.cache
                        .insert(host.clone(), (Instant::now(), parsed.clone()));
                    out.partitions.insert(host, parsed);
                }
                Err(f) => {
                    out.errors.insert(host, f.describe());
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept_all(_d: &str, _f: &str, _m: &str) -> (bool, bool) {
        (false, true)
    }

    const SAMPLE: &str = "Filesystem         1024-blocks      Used Available Capacity Mounted on\n\
        /dev/sda1            100 40 60 40% /\n\
        server:/exp          200 100 100 50% /mnt/My Data\n";

    #[test]
    fn parses_posix_with_space_in_mount() {
        let rows = parse_df_posix(SAMPLE, &accept_all);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].mountpoint, "/");
        assert_eq!(rows[0].size, 100 * 1024);
        assert_eq!(rows[1].mountpoint, "/mnt/My Data");
        assert_eq!(rows[1].used_pct, 50.0);
    }

    #[test]
    fn skips_header_and_garbage() {
        let rows = parse_df_posix(
            "Filesystem 1024-blocks Used Available Capacity Mounted on\njunk line\n",
            &accept_all,
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn skips_non_numeric() {
        let bad =
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x abc 1 2 3% /m\n";
        assert!(parse_df_posix(bad, &accept_all).is_empty());
    }

    #[test]
    fn malformed_df_rows_are_skipped() {
        let malformed = [
            "no capacity marker",
            "1 2 3 40% /mnt",
            "/dev/x 100 40 60 40%",
            "/dev/x invalid 40 60 40% /mnt",
            "/dev/x 100 invalid 60 40% /mnt",
            "/dev/x 100 40 invalid 40% /mnt",
            "/dev/x 100 40 60 invalid% /mnt",
        ];

        for line in malformed {
            assert!(parse_df_line(line).is_none(), "unexpectedly parsed: {line}");
        }
    }

    #[test]
    fn remote_filter_marks_rows_as_remote() {
        let rows = parse_df_posix(SAMPLE, &|_, _, _| (true, false));

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.is_remote));
    }

    #[test]
    fn filter_applies() {
        let rows = parse_df_posix(SAMPLE, &|_, _, _| (false, false));
        assert!(rows.is_empty());
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn tok() -> impl Strategy<Value = String> {
        "[a-zA-Z0-9_./:-]{1,20}"
    }

    proptest! {
        #[test]
        fn line_parser_never_panics(s in "\\PC{0,200}") {
            let _ = parse_df_line(&s);
        }

        #[test]
        fn wellformed_line_parses(
            dev in tok(), size in 1u64..1_000_000u64, used in 0u64..1_000_000u64,
            free in 0u64..1_000_000u64, pct in 0u64..100u64, mp in "/[a-z]{1,10}"
        ) {
            let line = format!("{dev} {size} {used} {free} {pct}% {mp}");
            let row = parse_df_line(&line).expect("parse ok");
            prop_assert_eq!(row.size, size * 1024);
            prop_assert_eq!(row.mountpoint, mp);
        }

        #[test]
        fn mount_with_spaces_roundtrips(mp in "[a-z]{1,5} [a-z]{1,5}") {
            let line = format!("/dev/x 10 4 6 40% {mp}");
            let row = parse_df_line(&line).expect("parse ok");
            prop_assert_eq!(row.mountpoint, mp);
        }
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;

    fn accept_all(_d: &str, _f: &str, _m: &str) -> (bool, bool) {
        (false, true)
    }

    #[tokio::test]
    async fn fetch_host_bogus_reports_failure() {
        let c = SshClient::new(0, Duration::from_millis(800), Duration::from_secs(30));
        let f = c
            .fetch_host("invalid-host-du-rs-xyz")
            .await
            .expect_err("host inválido falha");
        assert!(matches!(
            f.kind,
            SshFailureKind::Timeout | SshFailureKind::NonZero
        ));
        assert!(!f.describe().is_empty());
    }

    #[tokio::test]
    async fn fetch_all_reports_errors_per_host() {
        let c = SshClient::new(2, Duration::from_secs(2), Duration::from_secs(30));
        let empty = c.fetch_all(&[], &accept_all).await;
        assert!(empty.partitions.is_empty());
        assert!(empty.errors.is_empty());
        let out = c
            .fetch_all(&["invalid-host-du-rs-xyz".to_string()], &accept_all)
            .await;
        assert!(out.partitions.is_empty());
        assert!(
            out.errors.contains_key("invalid-host-du-rs-xyz"),
            "falha por host: {:?}",
            out.errors
        );
    }

    #[test]
    fn one_line_flattens_and_truncates() {
        assert_eq!(one_line("a\nb\tc", 20), "a b c");
        let long = one_line(&"x".repeat(300), 160);
        assert!(long.ends_with('…') && long.chars().count() == 161);
    }

    #[test]
    fn failure_describe_prefixes_kind() {
        let f = SshFailure {
            kind: SshFailureKind::NonZero,
            message: "ssh exit 255: boom".into(),
        };
        assert_eq!(f.describe(), "exit: ssh exit 255: boom");
    }
}
