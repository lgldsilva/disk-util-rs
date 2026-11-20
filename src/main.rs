//! `disk-util-rs`: monitor interativo de disco/RAM (port Rust de `disk_usage.py`).
//!
//! - Walk nativo jwalk+rayon (sem fork `du`), xdev, dedup hardlinks, redirect FUSE;
//! - RAM/discos via sysinfo (skip statvfs em `fuse.rclone`);
//! - SSH via fork `ssh host df -Pk` (sem russh), cache TTL;
//! - Config TOML dinâmica default<arquivo<env<CLI + hot reload (R/SIGHUP);
//! - Tick de coleta != render.

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use disk_util_rs::{
    app::{Browser, SortBy},
    cli::Cli,
    config::{default_config_path, example_config_path, generate_example_file, AppConfig},
    mounts::read_mounts,
    once::{render_report, OnceOpts},
    rclone::load_remotes_mapping,
    scan::{FolderScanner, ScannerConfig},
    ssh::SshClient,
    ssh_config,
    system::{self, SystemSnapshot},
    ui,
};

/// Monta `ScannerConfig` atual (mounts frescos a cada scan).
fn scanner_config(cfg: &AppConfig) -> ScannerConfig {
    ScannerConfig {
        walk_item: Duration::from_secs_f64(cfg.timeouts.walk_item_secs),
        walk_cache: Duration::from_secs_f64(cfg.timeouts.walk_cache_secs),
        cache_base: cfg.rclone.cache_base.clone(),
        remote_markers: cfg.partitions.remote_markers.clone(),
        mounts: read_mounts(),
    }
}

/// Hosts SSH efetivos: CLI (lista exata; `''` desativa) > config + descoberta
/// dinâmica do `~/.ssh/config` − `ignore_hosts`.
fn resolve_hosts(cli: &Cli, cfg: &AppConfig) -> Vec<String> {
    ssh_config::resolve_hosts(
        cli.ssh_hosts.as_deref(),
        &cfg.ssh_hosts,
        cfg.ssh.auto_discover,
        &cfg.ssh.ignore_hosts,
        &|| ssh_config::discover_hosts(&ssh_config::default_ssh_config_path()),
    )
}

fn ssh_client_for(cfg: &AppConfig) -> SshClient {
    SshClient::new_full(
        cfg.ssh.max_parallel,
        Duration::from_secs_f64(cfg.timeouts.ssh_secs),
        Duration::from_secs_f64(cfg.timeouts.ssh_cache_secs),
        cfg.ssh.strict_host_key_checking.clone(),
        cfg.ssh.connect_timeout_secs,
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);

    if cli.generate_config_example {
        let p = example_config_path();
        generate_example_file(&p)?;
        println!("Exemplo gerado em {}", p.display());
        return Ok(());
    }

    let cfg = AppConfig::load(&cli, &config_path).map_err(|e| {
        anyhow::anyhow!(
            "{} (dica: --generate-config-example p/ ver schema válido)",
            e
        )
    })?;
    let start_dir: PathBuf = cli.path.clone().unwrap_or_else(|| PathBuf::from(&cfg.path));
    let abs_dir = std::path::absolute(&start_dir).unwrap_or(start_dir);
    let hosts = resolve_hosts(&cli, &cfg);

    if cli.watch.is_none() || cli.once {
        return run_once(&cli, &cfg, &abs_dir, &hosts).await;
    }

    run_tui(cli, cfg, config_path, abs_dir).await
}

async fn run_once(
    cli: &Cli,
    cfg: &AppConfig,
    abs_dir: &Path,
    hosts: &[String],
) -> anyhow::Result<()> {
    let remotes = load_remotes_mapping(&cfg.rclone);
    let ssh = ssh_client_for(cfg);
    let filter = cfg.partitions.clone();
    let disks = disk_util_rs::disks::DiskMapCache::new(
        Duration::from_secs_f64(cfg.timeouts.sysinfo_secs),
        Duration::from_secs_f64(cfg.timeouts.disk_discover_secs),
    )
    .get()
    .await;
    let snap = system::collect(
        cfg,
        None,
        &ssh,
        hosts,
        &remotes,
        &|d, f, m| system::ssh_interesting(d, f, m, &filter),
        &disks,
    )
    .await;

    let mut items: Vec<disk_util_rs::scan::ScanItem> = Vec::new();
    if !cli.only_volumes {
        let mut scanner = FolderScanner::new();
        scanner.start_scan(abs_dir.to_path_buf(), scanner_config(cfg));
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            {
                let st = scanner.snapshot();
                if !st.scanning {
                    items = st.items.clone();
                    break;
                }
            }
            if Instant::now() > deadline {
                scanner.cancel();
                items = scanner.snapshot().items;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let browser = Browser::new(SortBy::Physical, true, String::new());
    let visible = browser.visible(&items);
    let out = render_report(
        &abs_dir.to_string_lossy(),
        cfg,
        &snap,
        &visible,
        &snap.mount_parents,
        &OnceOpts {
            show_boot: cli.show_boot || cfg.show_boot,
            cloud_size: cli.cloud_size,
            only_volumes: cli.only_volumes,
            only_folders: cli.only_folders,
        },
    );
    print!("{out}");
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn run_tui(
    cli: Cli,
    cfg: AppConfig,
    config_path: PathBuf,
    abs_dir: PathBuf,
) -> anyhow::Result<()> {
    let cfg_shared: Arc<Mutex<AppConfig>> = Arc::new(Mutex::new(cfg.clone()));
    let snap_shared: Arc<Mutex<SystemSnapshot>> = Arc::new(Mutex::new(SystemSnapshot::default()));
    let reload_flag = Arc::new(AtomicBool::new(false));

    // Coletor em background: tick de coleta != render.
    {
        let cfg_shared = cfg_shared.clone();
        let snap_shared = snap_shared.clone();
        let cli = cli.clone();
        let disk_cache = disk_util_rs::disks::DiskMapCache::new(
            Duration::from_secs_f64(cfg.timeouts.sysinfo_secs),
            Duration::from_secs_f64(cfg.timeouts.disk_discover_secs),
        );
        tokio::spawn(async move {
            let mut prev: Option<SystemSnapshot> = None;
            loop {
                let cfg = cfg_shared.lock().expect("cfg").clone();
                let remotes = load_remotes_mapping(&cfg.rclone);
                let hosts = resolve_hosts(&cli, &cfg);
                let ssh = ssh_client_for(&cfg);
                let disks = disk_cache.get().await;
                let filter = cfg.partitions.clone();
                let snap = system::collect(
                    &cfg,
                    prev.as_ref(),
                    &ssh,
                    &hosts,
                    &remotes,
                    &|d, f, m| system::ssh_interesting(d, f, m, &filter),
                    &disks,
                )
                .await;
                let delay = cfg.refresh_delay;
                prev = Some(snap.clone());
                *snap_shared.lock().expect("snap") = snap;
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            }
        });
    }

    // SIGHUP → hot reload (unix).
    #[cfg(unix)]
    {
        let flag = reload_flag.clone();
        tokio::spawn(async move {
            let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("sighup handler");
            loop {
                sig.recv().await;
                flag.store(true, Ordering::Relaxed);
            }
        });
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut prod_key = || -> Option<KeyEvent> {
        match event::poll(Duration::from_millis(150)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(k)) => Some(k),
                _ => None,
            },
            _ => None,
        }
    };
    let res = tui_loop(
        &cli,
        &cfg,
        &config_path,
        abs_dir,
        &cfg_shared,
        &snap_shared,
        &reload_flag,
        &mut terminal,
        &mut prod_key,
    )
    .await
    .map(|_| ());
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    res
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn tui_loop<B: ratatui::backend::Backend>(
    cli: &Cli,
    initial_cfg: &AppConfig,
    config_path: &Path,
    abs_dir: PathBuf,
    cfg_shared: &Arc<Mutex<AppConfig>>,
    snap_shared: &Arc<Mutex<SystemSnapshot>>,
    reload_flag: &Arc<AtomicBool>,
    terminal: &mut Terminal<B>,
    next_key: &mut dyn FnMut() -> Option<KeyEvent>,
) -> anyhow::Result<PathBuf>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    use disk_util_rs::app::{FilterOutcome, KeyAction, SearchOutcome};
    let mut cfg = initial_cfg.clone();
    let mut show_boot = cfg.show_boot;
    let mut browser = Browser::new(
        SortBy::parse(&cfg.sort_by).unwrap_or(SortBy::Physical),
        !cfg.ascending,
        cfg.filter.clone(),
    );
    let mut scanner = FolderScanner::new();
    let mut target = abs_dir;
    scanner.start_scan(target.clone(), scanner_config(&cfg));
    let mut status: Option<(String, Instant)> = None;
    let mut saved_filter = String::new();
    let mut ssh_err_open = false;

    loop {
        // Hot reload: R ou SIGHUP.
        if reload_flag.swap(false, Ordering::Relaxed) {
            match AppConfig::load(cli, config_path) {
                Ok(fresh) => {
                    show_boot = fresh.show_boot;
                    *cfg_shared.lock().expect("cfg") = fresh.clone();
                    cfg = fresh;
                    status = Some(("config recarregada".to_string(), Instant::now()));
                    scanner.start_scan(target.clone(), scanner_config(&cfg));
                }
                Err(e) => {
                    status = Some((format!("reload falhou: {e:#}"), Instant::now()));
                }
            }
        }
        if let Some((_, t)) = &status {
            if t.elapsed() > Duration::from_secs(4) {
                status = None;
            }
        }

        let snap_guard = snap_shared.lock().expect("snap").clone();
        let scan_guard = scanner.snapshot();
        let visible = browser.visible(&scan_guard.items);
        browser.anchor(&visible);
        let selected = browser.selected_idx;
        let status_str = status.as_ref().map(|(s, _)| s.as_str());
        terminal.draw(|f| {
            ui::draw(
                f,
                ui::UiView {
                    title_dir: &target.to_string_lossy(),
                    snap: &snap_guard,
                    items: visible.clone(),
                    browser: &browser,
                    cfg: &cfg,
                    remotes_parents: &snap_guard.mount_parents,
                    show_boot,
                    only_volumes: cli.only_volumes,
                    only_folders: cli.only_folders,
                    cloud_size: cli.cloud_size,
                    status: status_str,
                    ssh_errors_open: ssh_err_open,
                },
                (
                    scan_guard.scanning,
                    scan_guard.scanned_count,
                    scan_guard.total_count,
                ),
                selected,
            );
        })?;
        let has_ssh_errors = !snap_guard.ssh_errors.is_empty();
        drop(snap_guard);
        drop(scan_guard);

        let Some(key) = next_key() else {
            tokio::time::sleep(Duration::from_millis(10)).await;
            continue;
        };

        // Popup de falhas SSH aberto: Esc fecha (antes da navegação).
        if ssh_err_open && key.code == KeyCode::Esc {
            ssh_err_open = false;
            continue;
        }

        // --- modos de edição (filtro/busca) consomem a tecla ---
        if browser.filter_mode {
            match browser.filter_key(key.code, &saved_filter.clone()) {
                FilterOutcome::Continue | FilterOutcome::Done | FilterOutcome::Cancelled => {}
            }
            continue;
        }
        if browser.search_mode {
            match browser.search_key(key.code, &browser.visible(&scanner.snapshot().items)) {
                SearchOutcome::Continue | SearchOutcome::Done => continue,
                SearchOutcome::Navigate => {} // cai p/ navegação abaixo
            }
        }

        match browser.classify(key.code, &cfg.keybindings) {
            KeyAction::Quit => break,
            KeyAction::MoveUp => {
                let st = scanner.snapshot();
                browser.move_up(&browser.visible(&st.items));
            }
            KeyAction::MoveDown => {
                let st = scanner.snapshot();
                browser.move_down(&browser.visible(&st.items));
            }
            KeyAction::EnterDir => {
                let st = scanner.snapshot();
                let vis = browser.visible(&st.items);
                if let Some(item) = vis.get(browser.selected_idx) {
                    if item.is_dir {
                        target = item.path.clone();
                        browser.selected_idx = 0;
                        browser.selected_name = None;
                        scanner.start_scan(target.clone(), scanner_config(&cfg));
                    }
                }
            }
            KeyAction::GoParent => {
                if let Some(parent) = target.parent().map(|p| p.to_path_buf()) {
                    let prev_name = target.file_name().map(|n| n.to_string_lossy().into_owned());
                    target = parent;
                    browser.selected_idx = 0;
                    browser.selected_name = prev_name;
                    scanner.start_scan(target.clone(), scanner_config(&cfg));
                }
            }
            KeyAction::CloseHelp => browser.show_help = false,
            KeyAction::StartSearch => {
                browser.search_mode = true;
                browser.search_query.clear();
            }
            KeyAction::StartFilter => {
                browser.filter_mode = true;
                saved_filter = browser.filter.clone();
                browser.filter_query.clone_from(&browser.filter);
            }
            KeyAction::CycleSort => browser.cycle_sort(),
            KeyAction::ToggleOrder => browser.sort_desc = !browser.sort_desc,
            KeyAction::Rescan => scanner.start_scan(target.clone(), scanner_config(&cfg)),
            KeyAction::ToggleBoot => show_boot = !show_boot,
            KeyAction::ToggleHelp => browser.show_help = !browser.show_help,
            KeyAction::Reload => reload_flag.store(true, Ordering::Relaxed),
            KeyAction::SshErrors => {
                if has_ssh_errors {
                    ssh_err_open = !ssh_err_open;
                } else {
                    status = Some(("sem falhas de conexão SSH".to_string(), Instant::now()));
                }
            }
            KeyAction::Unhandled => {}
        }
    }
    Ok(target)
}

#[cfg(test)]
mod bin_tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use std::collections::VecDeque;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn screen_text(t: &Terminal<TestBackend>) -> String {
        t.backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("")
    }

    fn fixture(name: &str) -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix(&format!("du-tui-{name}-"))
            .tempdir()
            .unwrap();
        std::fs::create_dir_all(dir.path().join("big")).unwrap();
        std::fs::write(dir.path().join("big").join("data.bin"), vec![3u8; 2 << 20]).unwrap();
        std::fs::write(dir.path().join("small.txt"), vec![4u8; 512]).unwrap();
        dir
    }

    fn test_cli(extra: &[&str]) -> Cli {
        let mut args = vec!["disk-util-rs", "--ssh-hosts", ""];
        args.extend_from_slice(extra);
        Cli::try_parse_from(args).expect("parse")
    }

    /// Estado compartilhado do loop TUI p/ testes headless (TestBackend).
    type TestEnv = (
        AppConfig,
        PathBuf,
        Arc<Mutex<AppConfig>>,
        Arc<Mutex<SystemSnapshot>>,
        Arc<AtomicBool>,
    );

    fn test_env(dir: &std::path::Path, cli: &Cli) -> TestEnv {
        let cfg_path = dir.join("cfg.toml");
        std::fs::write(&cfg_path, "").unwrap();
        let loaded = AppConfig::load(cli, &cfg_path).expect("load");
        (
            loaded.clone(),
            cfg_path,
            Arc::new(Mutex::new(loaded)),
            Arc::new(Mutex::new(SystemSnapshot::default())),
            Arc::new(AtomicBool::new(false)),
        )
    }

    async fn run_script(
        cli: &Cli,
        dir: &std::path::Path,
        keys: Vec<KeyCode>,
        extra_cfg: Option<&str>,
    ) -> (anyhow::Result<PathBuf>, String) {
        let (cfg, cfg_path, cfg_shared, snap_shared, reload_flag) = test_env(dir, cli);
        if let Some(text) = extra_cfg {
            std::fs::write(&cfg_path, text).unwrap();
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        let mut script: VecDeque<KeyEvent> = keys.into_iter().map(key).collect();
        let mut first = true;
        let mut last_rescan = false;
        let mut next = move || {
            // Dá tempo p/ o scan inicial da fixture concluir antes do Enter.
            if first {
                first = false;
                std::thread::sleep(Duration::from_millis(800));
            } else if last_rescan {
                // Rescan (r/R/Enter/Backspace) republica a lista; espera assentar.
                std::thread::sleep(Duration::from_millis(600));
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
            let k = script.pop_front();
            last_rescan = matches!(
                k.map(|e| e.code),
                Some(KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::Enter | KeyCode::Backspace)
            );
            k
        };
        let res = tokio::time::timeout(
            Duration::from_secs(60),
            tui_loop(
                cli,
                &cfg,
                &cfg_path,
                dir.to_path_buf(),
                &cfg_shared,
                &snap_shared,
                &reload_flag,
                &mut terminal,
                &mut next,
            ),
        )
        .await
        .expect("tui não pode travar");
        let screen = screen_text(&terminal);
        (res, screen)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn scripted_nav_search_filter_reload_enter_back_quit() {
        let dir = fixture("nav");
        let cli = test_cli(&[]);
        let (res, screen) = run_script(
            &cli,
            dir.path(),
            vec![
                KeyCode::Down,
                KeyCode::Up,
                KeyCode::Char('o'),
                KeyCode::Char('s'),
                KeyCode::Char('f'),
                KeyCode::Char('b'),
                KeyCode::Enter,
                KeyCode::Char('f'),
                KeyCode::Esc,
                KeyCode::Char('/'),
                KeyCode::Char('s'),
                KeyCode::Esc,
                // Navega com o scan inicial já assentado (sem rescan imediatamente antes).
                KeyCode::Enter,
                KeyCode::Backspace,
                KeyCode::Char('r'),
                KeyCode::Char('b'),
                KeyCode::Char('h'),
                KeyCode::Char('h'),
                KeyCode::Char('R'),
                KeyCode::Char('q'),
            ],
            None,
        )
        .await;
        let target = res.expect("tui ok");
        assert_eq!(target, dir.path(), "Enter+Backspace voltam à raiz");
        assert!(screen.contains("Navegador"), "tela final tem navegador");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn ssh_errors_key_shows_status_when_no_failures() {
        let dir = fixture("ssh-err");
        // Sem hosts SSH (test_cli desativa) não há falhas: 'd' mostra status.
        let cli = test_cli(&[]);
        let (res, screen) = run_script(
            &cli,
            dir.path(),
            vec![KeyCode::Char('d'), KeyCode::Char('q')],
            None,
        )
        .await;
        assert!(res.is_ok());
        assert!(
            screen.contains("sem falhas de conexão SSH"),
            "status do 'd' sem falhas: {screen}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reload_failure_keeps_running() {
        let dir = fixture("badcfg");
        let cli = test_cli(&[]);
        let (res, _screen) = run_script(
            &cli,
            dir.path(),
            vec![KeyCode::Char('R'), KeyCode::Char('q')],
            Some("campo_inexistente = 1\n"),
        )
        .await;
        assert!(res.is_ok(), "reload falho não derruba a TUI");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn reload_success_applies_config_and_keeps_running() {
        let dir = fixture("goodcfg");
        let cli = test_cli(&[]);
        let (res, screen) = run_script(
            &cli,
            dir.path(),
            vec![KeyCode::Char('R'), KeyCode::Char('q')],
            Some("show_boot = true\n"),
        )
        .await;

        assert!(res.is_ok(), "reload válido mantém a TUI funcionando");
        assert!(screen.contains("config recarregada"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn only_volumes_and_only_folders_render() {
        let dir = fixture("modes");
        for (extra, marker, anti) in [
            (
                vec!["--only-volumes"],
                "Discos Locais",
                "Navegador de Diretórios:",
            ),
            (vec!["--only-folders"], "Navegador", "Memória RAM"),
        ] {
            let cli = test_cli(&extra);
            let (res, screen) = run_script(&cli, dir.path(), vec![KeyCode::Char('q')], None).await;
            assert!(res.is_ok());
            assert!(screen.contains(marker), "modo {extra:?} sem {marker}");
            assert!(!screen.contains(anti), "modo {extra:?} vazou {anti}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_once_writes_report() {
        let dir = fixture("once");
        let cli = test_cli(&["--once"]);
        let cfg = AppConfig::default();
        // test_cli passa --ssh-hosts '': lista exata vazia, sem descoberta.
        let hosts = resolve_hosts(&cli, &cfg);
        assert!(hosts.is_empty());
        run_once(&cli, &cfg, dir.path(), &hosts)
            .await
            .expect("once ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn run_once_only_volumes_skips_folder_scan() {
        let dir = fixture("volumes-only-once");
        let cli = test_cli(&["--once", "--only-volumes"]);
        let cfg = AppConfig::default();
        let hosts = resolve_hosts(&cli, &cfg);

        run_once(&cli, &cfg, dir.path(), &hosts)
            .await
            .expect("volume-only report");
    }

    #[test]
    fn helpers_match_config() {
        let discover = || Vec::new();
        assert_eq!(
            ssh_config::resolve_hosts(None, &["h".to_string()], false, &[], &discover),
            vec!["h".to_string()]
        );
        assert_eq!(
            ssh_config::resolve_hosts(
                Some(" host-a, ,host-b "),
                &["ignored".to_string()],
                true,
                &[],
                &discover
            ),
            vec!["host-a".to_string(), "host-b".to_string()]
        );
        assert!(
            ssh_config::resolve_hosts(Some(""), &["h".to_string()], true, &[], &discover)
                .is_empty()
        );
        let cfg = AppConfig::default();
        let ssh = ssh_client_for(&cfg);
        assert_eq!(ssh.timeout, Duration::from_secs_f64(cfg.timeouts.ssh_secs));
        assert_eq!(
            ssh.cache_ttl,
            Duration::from_secs_f64(cfg.timeouts.ssh_cache_secs)
        );
        let sc = scanner_config(&cfg);
        assert_eq!(sc.remote_markers, cfg.partitions.remote_markers);
        assert!(!sc.mounts.is_empty());
    }
}
