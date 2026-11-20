//! Render ratatui: RAM, volumes, SSH, navegador, footer.
//!
//! Tick de coleta != render: esta função só lê snapshots prontos.

use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame,
};

use crate::app::Browser;
use crate::config::AppConfig;
use crate::format::{
    bar10, disk_level, format_size, format_size_short, ram_level, trunc_label, Level,
};
use crate::scan::{ItemStatus, ScanItem};
use crate::system::{DiskGroupView, PartitionView, SystemSnapshot, VolumeUsage};

pub struct UiView<'a> {
    pub title_dir: &'a str,
    pub snap: &'a SystemSnapshot,
    pub items: Vec<&'a ScanItem>,
    pub browser: &'a Browser,
    pub cfg: &'a AppConfig,
    pub remotes_parents: &'a std::collections::HashMap<String, String>,
    pub show_boot: bool,
    pub only_volumes: bool,
    pub only_folders: bool,
    pub cloud_size: bool,
    pub status: Option<&'a str>,
    /// Popup de detalhes das falhas de conexão SSH aberto (tecla `d`).
    pub ssh_errors_open: bool,
}

fn level_color(l: Level) -> Color {
    match l {
        Level::Crit => Color::Red,
        Level::Warn => Color::Yellow,
        Level::Ok => Color::Green,
    }
}

fn level_style(l: Level) -> Style {
    let mut s = Style::default().fg(level_color(l));
    if l == Level::Crit {
        s = s.add_modifier(Modifier::BOLD);
    }
    s
}

/// HH:MM:SS UTC (sem dep externa de timezone; golden mascara o valor).
pub fn utc_hms() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02} UTC", s / 3600, (s % 3600) / 60, s % 60)
}

fn decimal(cfg: &AppConfig) -> bool {
    cfg.display.byte_format == "decimal"
}

fn precision(cfg: &AppConfig) -> usize {
    cfg.display.precision
}

fn ram_rows(snap: &SystemSnapshot, cfg: &AppConfig) -> Vec<Row<'static>> {
    let mut rows = Vec::new();
    let t = &cfg.thresholds;
    if snap.ram_total > 0 {
        let lvl = ram_level(snap.ram_pct, t.ram_warn, t.ram_crit);
        rows.push(Row::new(vec![
            Cell::from("RAM"),
            Cell::from(format!(
                "T:{}",
                format_size_short(snap.ram_total, decimal(cfg))
            )),
            Cell::from(format!(
                "U:{}",
                format_size_short(snap.ram_used, decimal(cfg))
            )),
            Cell::from(format!(
                "L:{}",
                format_size_short(snap.ram_avail, decimal(cfg))
            )),
            Cell::from(Span::styled(bar10(snap.ram_pct), level_style(lvl))),
            Cell::from(Span::styled(
                format!("{:.1}%", snap.ram_pct),
                level_style(lvl),
            )),
        ]));
    }
    if snap.swap_total > 0 {
        let lvl = ram_level(snap.swap_pct, t.ram_warn, t.ram_crit);
        rows.push(Row::new(vec![
            Cell::from("SWAP"),
            Cell::from(format!(
                "T:{}",
                format_size_short(snap.swap_total, decimal(cfg))
            )),
            Cell::from(format!(
                "U:{}",
                format_size_short(snap.swap_used, decimal(cfg))
            )),
            Cell::from(format!(
                "L:{}",
                format_size_short(snap.swap_free, decimal(cfg))
            )),
            Cell::from(Span::styled(bar10(snap.swap_pct), level_style(lvl))),
            Cell::from(Span::styled(
                format!("{:.1}%", snap.swap_pct),
                level_style(lvl),
            )),
        ]));
    } else {
        rows.push(Row::new(vec![Cell::from("SWAP"), Cell::from("Desativada")]));
    }
    rows
}

fn totalizer_row(
    name: &str,
    total: u64,
    used: u64,
    free: u64,
    t: &crate::config::Thresholds,
    cfg: &AppConfig,
) -> Option<Row<'static>> {
    if total == 0 {
        return None;
    }
    let pct = used as f64 / total as f64 * 100.0;
    let lvl = disk_level(pct, t.disk_warn, t.disk_crit);
    Some(Row::new(vec![
        Cell::from(Span::styled(
            name.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            format_size_short(total, decimal(cfg)),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            format_size_short(used, decimal(cfg)),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            format_size_short(free, decimal(cfg)),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Cell::from(Span::styled(
            format!("[{}] {:.1}%", bar10(pct), pct),
            level_style(lvl),
        )),
    ]))
}

/// Linha de mount remoto (rclone: fake via cache/☁ ou cota do remote pai).
fn remote_row(p: &PartitionView, u: &VolumeUsage, v: &UiView) -> Row<'static> {
    let snap = v.snap;
    let cfg = v.cfg;
    let label = trunc_label(
        &format!(
            "{}{}",
            p.mountpoint,
            if p.remote { " [rclone]" } else { "" }
        ),
        24,
    );
    if u.fake {
        // Backend sem cota: lógico do cache com --cloud-size, senão "—".
        let used_s = if v.cloud_size {
            let rn = crate::rclone::remote_name_from_device(&p.device);
            match crate::system::cache_apparent_for(rn, snap) {
                Some(a) if a > 0 && a < cfg.rclone.fake_size_threshold => {
                    format_size_short(a, true)
                }
                _ => "—".to_string(),
            }
        } else if u.used > 0 && u.used < cfg.rclone.fake_size_threshold {
            format_size_short(u.used, true)
        } else {
            "—".to_string()
        };
        let parent_note = v
            .remotes_parents
            .get(&p.mountpoint)
            .map(|s| format!(" [{s}]"))
            .unwrap_or_default();
        return Row::new(vec![
            Cell::from(label),
            Cell::from("—"),
            Cell::from(used_s),
            Cell::from("—"),
            Cell::from(format!("☁{parent_note}")),
        ]);
    }
    let mut free = u.free;
    let mut shared = String::new();
    if let Some(parent) = v.remotes_parents.get(&p.mountpoint) {
        let n = snap.remotes_parent_count.get(parent).copied().unwrap_or(0);
        if n > 1 {
            shared = format!(" [{parent}]");
            if let (Some(tot), Some(sum)) = (
                snap.parent_totals.get(parent),
                snap.parent_used_sum.get(parent),
            ) {
                free = tot.saturating_sub(*sum);
            }
        }
    }
    let used_pct = if u.total > 0 {
        u.used as f64 / u.total as f64 * 100.0
    } else {
        0.0
    };
    let lvl = disk_level(used_pct, cfg.thresholds.disk_warn, cfg.thresholds.disk_crit);
    Row::new(vec![
        Cell::from(label),
        Cell::from(format_size_short(u.total, p.remote)),
        Cell::from(format_size_short(u.used, p.remote)),
        Cell::from(format_size_short(free, p.remote)),
        Cell::from(Span::styled(
            format!("[{}] {:.1}%{shared}", bar10(used_pct), used_pct),
            level_style(lvl),
        )),
    ])
}

/// Linha de mount local (comportamento legado de group_by="device").
fn local_row(p: &PartitionView, u: &VolumeUsage, v: &UiView) -> Row<'static> {
    let cfg = v.cfg;
    let used_pct = if u.total > 0 {
        u.used as f64 / u.total as f64 * 100.0
    } else {
        0.0
    };
    let lvl = disk_level(used_pct, cfg.thresholds.disk_warn, cfg.thresholds.disk_crit);
    Row::new(vec![
        Cell::from(trunc_label(&p.mountpoint, 24)),
        Cell::from(format_size_short(u.total, false)),
        Cell::from(format_size_short(u.used, false)),
        Cell::from(format_size_short(u.free, false)),
        Cell::from(Span::styled(
            format!("[{}] {:.1}%", bar10(used_pct), used_pct),
            level_style(lvl),
        )),
    ])
}

/// Linha consolidada por disco físico/pool ZFS: `sda (/, /boot)`.
fn disk_group_row(g: &DiskGroupView, v: &UiView) -> Option<Row<'static>> {
    let mps: Vec<&str> = g
        .mountpoints
        .iter()
        .map(String::as_str)
        .filter(|mp| v.show_boot || !crate::format::is_boot_mount(mp, &v.cfg.boot.paths))
        .collect();
    if mps.is_empty() {
        return None;
    }
    let cfg = v.cfg;
    let label = trunc_label(&format!("{} ({})", g.label, mps.join(", ")), 24);
    let used_pct = if g.total > 0 {
        g.used as f64 / g.total as f64 * 100.0
    } else {
        0.0
    };
    let lvl = disk_level(used_pct, cfg.thresholds.disk_warn, cfg.thresholds.disk_crit);
    Some(Row::new(vec![
        Cell::from(label),
        Cell::from(format_size_short(g.total, decimal(cfg))),
        Cell::from(format_size_short(g.used, decimal(cfg))),
        Cell::from(format_size_short(g.free, decimal(cfg))),
        Cell::from(Span::styled(
            format!("[{}] {:.1}%", bar10(used_pct), used_pct),
            level_style(lvl),
        )),
    ]))
}

fn volumes_rows(v: &UiView) -> Vec<Row<'static>> {
    let snap = v.snap;
    let cfg = v.cfg;
    let t = &cfg.thresholds;
    let mut rows = Vec::new();
    let mut parts: Vec<_> = snap.partitions.iter().collect();
    parts.sort_by(|a, b| a.mountpoint.cmp(&b.mountpoint));
    let boot_visible =
        |mp: &str| v.show_boot || !crate::format::is_boot_mount(mp, &v.cfg.boot.paths);

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
                rows.push(remote_row(p, u, v));
            } else {
                rows.push(local_row(p, u, v));
            }
        }
    } else {
        // Consolidado: uma linha por disco físico/pool + remotos por mount.
        for g in &snap.disk_groups {
            if let Some(r) = disk_group_row(g, v) {
                rows.push(r);
            }
        }
        for p in parts {
            if !p.remote || !boot_visible(&p.mountpoint) {
                continue;
            }
            let Some(u) = snap.usages.get(&p.mountpoint) else {
                continue;
            };
            rows.push(remote_row(p, u, v));
        }
    }
    // Separador visual + totalizadores.
    rows.push(Row::new(vec![Cell::from("─".repeat(12))]));
    for (name, total, used, free) in [
        (
            "FÍSICO",
            snap.total_physical,
            snap.used_physical,
            snap.free_physical,
        ),
        ("NUVEM", snap.total_cloud, snap.used_cloud, snap.free_cloud),
        (
            "VIRTUAL",
            snap.total_virtual,
            snap.used_virtual,
            snap.free_virtual,
        ),
    ] {
        if let Some(r) = totalizer_row(name, total, used, free, t, cfg) {
            rows.push(r);
        }
    }
    if snap.rclone_cache_total > 0 {
        let details: Vec<String> = snap
            .cache_details
            .iter()
            .take(3)
            .map(|c| {
                format!(
                    "{}:{}",
                    c.name,
                    format_size_short(c.allocated, decimal(cfg))
                )
            })
            .collect();
        let mut d = details.join(" | ");
        if snap.cache_details.len() > 3 {
            d.push_str(" ...");
        }
        rows.push(Row::new(vec![
            Cell::from(Span::styled(
                format!(
                    "Cache rclone: {}",
                    format_size_short(snap.rclone_cache_total, decimal(cfg))
                ),
                Style::default().fg(Color::Yellow),
            )),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(Span::styled(d, Style::default().fg(Color::DarkGray))),
        ]));
    }
    rows
}

fn ssh_rows(v: &UiView) -> Vec<Row<'static>> {
    let mut rows = Vec::new();
    // União ok+falha, ordenada: host vivo mostra partições; morto mostra a
    // falha (vermelho) com atalho `d` p/ detalhe.
    let mut hosts: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    hosts.extend(v.snap.remote_hosts.keys().map(String::as_str));
    hosts.extend(v.snap.ssh_errors.keys().map(String::as_str));
    for host in hosts {
        if let Some(parts) = v.snap.remote_hosts.get(host) {
            rows.push(Row::new(vec![Cell::from(Span::styled(
                format!("Host: {host}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))]));
            for p in parts {
                if !v.show_boot && crate::format::is_boot_mount(&p.mountpoint, &v.cfg.boot.paths) {
                    continue;
                }
                let label = trunc_label(
                    &format!(
                        "{}{}",
                        p.mountpoint,
                        if p.is_remote { " [rclone]" } else { "" }
                    ),
                    24,
                );
                let fake = p.size >= v.cfg.rclone.fake_size_threshold
                    || p.free >= v.cfg.rclone.fake_size_threshold
                    || p.size == 0;
                if p.is_remote && fake {
                    rows.push(Row::new(vec![
                        Cell::from(format!("  {label}")),
                        Cell::from("—"),
                        Cell::from("—"),
                        Cell::from("—"),
                        Cell::from("☁"),
                    ]));
                    continue;
                }
                let lvl = disk_level(
                    p.used_pct,
                    v.cfg.thresholds.disk_warn,
                    v.cfg.thresholds.disk_crit,
                );
                rows.push(Row::new(vec![
                    Cell::from(format!("  {label}")),
                    Cell::from(format_size_short(p.size, p.is_remote)),
                    Cell::from(format_size_short(p.used, p.is_remote)),
                    Cell::from(format_size_short(p.free, p.is_remote)),
                    Cell::from(Span::styled(
                        format!("[{}] {:.1}%", bar10(p.used_pct), p.used_pct),
                        level_style(lvl),
                    )),
                ]));
            }
        } else if let Some(err) = v.snap.ssh_errors.get(host) {
            rows.push(Row::new(vec![Cell::from(Span::styled(
                format!("Host: {host} — falhou ({err})"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ))]));
        }
    }
    rows
}

/// Linhas do popup de detalhes das falhas de conexão (`d`).
fn ssh_error_rows(v: &UiView) -> Vec<Row<'static>> {
    let mut hosts: Vec<_> = v.snap.ssh_errors.iter().collect();
    hosts.sort_by(|a, b| a.0.cmp(b.0));
    hosts
        .into_iter()
        .map(|(host, err)| {
            Row::new(vec![
                Cell::from(Span::styled(
                    host.clone(),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
                Cell::from(err.clone()),
            ])
        })
        .collect()
}

fn ratio_bar(ratio: f64) -> (String, Color) {
    let n = (ratio.clamp(0.0, 1.0) * 8.0) as usize;
    let color = if ratio >= 0.85 {
        Color::Red
    } else if ratio >= 0.50 {
        Color::Yellow
    } else {
        Color::Green
    };
    (format!("{}{}", "█".repeat(n), "░".repeat(8 - n)), color)
}

fn browser_rows(v: &UiView, max_phys: u64) -> Vec<Row<'static>> {
    let mut rows = Vec::new();
    for item in &v.items {
        let (phys_s, log_s) = match item.status {
            ItemStatus::Calculating => ("calculando...".to_string(), "calculando...".to_string()),
            ItemStatus::Timeout => ("timeout".to_string(), String::new()),
            ItemStatus::Error => ("erro".to_string(), String::new()),
            ItemStatus::Ok => (
                format_size(item.size_physical, decimal(v.cfg), precision(v.cfg)),
                format_size(item.size_logical, decimal(v.cfg), precision(v.cfg)),
            ),
        };
        let (bar, color) = if max_phys > 0 && item.size_physical > 0 {
            ratio_bar(item.size_physical as f64 / max_phys as f64)
        } else {
            (String::new(), Color::Green)
        };
        let name = if item.is_dir {
            format!("{}/", item.name)
        } else {
            item.name.clone()
        };
        rows.push(Row::new(vec![
            Cell::from(""),
            Cell::from(name),
            Cell::from(if item.is_dir { "Pasta" } else { "Arquivo" }.to_string()),
            Cell::from(phys_s),
            Cell::from(log_s),
            Cell::from(Span::styled(bar, Style::default().fg(color))),
        ]));
    }
    if rows.is_empty() {
        // Mensagem na coluna do nome (a coluna 0 do cursor tem largura 1).
        rows.push(Row::new(vec![
            Cell::from(""),
            Cell::from("Pasta vazia ou nenhum item corresponde ao filtro."),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ]));
    }
    rows
}

fn help_rows() -> Vec<Row<'static>> {
    [
        ("Setas Up/Down", "Move o cursor de seleção"),
        ("Enter", "Entra no diretório selecionado"),
        ("Backspace/Left", "Sobe para o diretório pai"),
        ("s", "Inverte a ordenação (Ascendente / Descendente)"),
        (
            "o",
            "Rotaciona ordenação (Físico -> Lógico -> Nome -> Tipo)",
        ),
        ("r", "Recarrega a pasta atual"),
        ("f", "Filtra itens pelo nome (esconde os que não casam)"),
        ("/", "Busca incremental: digite e o cursor pula para o item"),
        (
            "d",
            "Detalha as falhas de conexão SSH (host, motivo, stderr)",
        ),
        ("b", "Mostra/oculta as partições de boot (/boot, /boot/efi)"),
        ("R", "Recarrega config TOML (hot reload; também via SIGHUP)"),
        ("h", "Fecha esta ajuda"),
        ("q", "Sai do programa"),
    ]
    .iter()
    .map(|(k, d)| {
        Row::new(vec![
            Cell::from(Span::styled(
                k.to_string(),
                Style::default().fg(Color::Cyan),
            )),
            Cell::from(d.to_string()),
        ])
    })
    .collect()
}

fn footer_line(v: &UiView, scanning: (bool, usize, usize)) -> Line<'static> {
    let b = v.browser;
    let mut parts: Vec<Span> = vec![
        Span::styled("Atalhos: ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(format!(
            "Filtro:{} | ",
            if v.browser.filter.is_empty() {
                "nenhum [f]".to_string()
            } else {
                format!("'{}'", v.browser.filter)
            }
        )),
        Span::raw(format!(
            "ordem:{} {} | ",
            b.sort_by.label(),
            if b.sort_desc { "desc" } else { "asc" }
        )),
    ];
    if b.search_mode {
        parts.push(Span::styled(
            format!("Buscar: {}▏ ", b.search_query),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if b.filter_mode {
        parts.push(Span::styled(
            format!("Filtro: {}▏ ", b.filter_query),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if scanning.0 {
        parts.push(Span::styled(
            format!("Varrendo: {}/{} ", scanning.1, scanning.2),
            Style::default().fg(Color::Yellow),
        ));
    }
    if let Some(s) = v.status {
        parts.push(Span::styled(
            s.to_string(),
            Style::default().fg(Color::Cyan),
        ));
    }
    if !v.snap.ssh_errors.is_empty() {
        parts.push(Span::styled(
            "[d] erros ssh | ".to_string(),
            Style::default().fg(Color::Red),
        ));
    }
    parts.push(Span::raw(
        "[Enter] entrar [Bksp] voltar [o/s] ordem [r] reload [b] boot [h] ajuda [q] sair",
    ));
    Line::from(parts)
}

pub fn draw(f: &mut Frame, v: UiView, scan_state: (bool, usize, usize), selected: usize) {
    let area = f.area();
    let header = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let header_text = format!(
        "disk-util-rs | Diretório: {} | Hora: {}",
        v.title_dir,
        utc_hms()
    );
    let ram_block = Block::default()
        .title("Memória RAM & Swap")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue));
    let vol_block = Block::default()
        .title("Discos Locais")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue));

    let vol_rows = volumes_rows(&v);
    let ssh_rows_vec = ssh_rows(&v);
    let ram_h: u16 = (ram_rows(v.snap, v.cfg).len() + 2).try_into().unwrap_or(4);
    let vol_h: u16 = (vol_rows.len() + 3)
        .try_into()
        .unwrap_or(8)
        .min(area.height / 2 + 8);
    let ssh_h: u16 = if ssh_rows_vec.is_empty() {
        0
    } else {
        (ssh_rows_vec.len() + 3).try_into().unwrap_or(5).min(12)
    };

    // only_folders: só header+navegador+footer (navegador com a tela toda).
    if v.only_folders {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(0),
                Constraint::Length(3),
            ])
            .split(area);
        f.render_widget(Paragraph::new(header_text).block(header), chunks[0]);
        render_browser(f, chunks[1], &v, scan_state, selected);
        f.render_widget(
            Paragraph::new(footer_line(&v, scan_state))
                .block(Block::default().title("Controles").borders(Borders::ALL)),
            chunks[2],
        );
        return;
    }

    let mut constraints = vec![
        Constraint::Length(3),
        Constraint::Length(ram_h),
        Constraint::Length(vol_h),
    ];
    if ssh_h > 0 {
        constraints.push(Constraint::Length(ssh_h));
    }
    constraints.push(Constraint::Min(0));
    constraints.push(Constraint::Length(3));

    // only_volumes: sem navegador; only_folders: só header+navegador+footer.
    if v.only_volumes {
        let mut constraints = vec![
            Constraint::Length(3),
            Constraint::Length(ram_h),
            Constraint::Min(0),
        ];
        if ssh_h > 0 {
            constraints.push(Constraint::Length(ssh_h));
        }
        constraints.push(Constraint::Length(3));
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);
        f.render_widget(Paragraph::new(header_text).block(header), chunks[0]);
        let ram_table = Table::new(
            ram_rows(v.snap, v.cfg),
            [
                Constraint::Length(6),
                Constraint::Length(11),
                Constraint::Length(9),
                Constraint::Length(9),
                Constraint::Length(16),
                Constraint::Length(7),
            ],
        )
        .block(ram_block);
        f.render_widget(ram_table, chunks[1]);
        let widths = [
            Constraint::Min(10),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(22),
        ];
        let vt = Table::new(vol_rows, widths)
            .header(
                Row::new(vec!["Montagem", "Total", "Usado", "Livre", "Uso"]).style(
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
            )
            .block(vol_block);
        f.render_widget(vt, chunks[2]);
        let mut idx = 3;
        if ssh_h > 0 {
            let ssh_block = Block::default()
                .title("Servidores Remotos (SSH)")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan));
            let st = Table::new(
                ssh_rows_vec,
                [
                    Constraint::Min(14),
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Length(22),
                ],
            )
            .block(ssh_block);
            f.render_widget(st, chunks[idx]);
            idx += 1;
        }
        f.render_widget(
            Paragraph::new(footer_line(&v, scan_state))
                .block(Block::default().title("Controles").borders(Borders::ALL)),
            chunks[idx],
        );
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let mut idx = 0;
    f.render_widget(Paragraph::new(header_text).block(header), chunks[idx]);
    idx += 1;

    let ram_table = Table::new(
        ram_rows(v.snap, v.cfg),
        [
            Constraint::Length(6),
            Constraint::Length(11),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(16),
            Constraint::Length(7),
        ],
    )
    .block(ram_block);
    f.render_widget(ram_table, chunks[idx]);
    idx += 1;

    let widths = [
        Constraint::Min(10),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(22),
    ];
    let vt = Table::new(vol_rows, widths)
        .header(
            Row::new(vec!["Montagem", "Total", "Usado", "Livre", "Uso"]).style(
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(vol_block);
    f.render_widget(vt, chunks[idx]);
    idx += 1;

    if ssh_h > 0 {
        let ssh_block = Block::default()
            .title("Servidores Remotos (SSH)")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        let st = Table::new(
            ssh_rows_vec,
            [
                Constraint::Min(14),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(22),
            ],
        )
        .block(ssh_block);
        f.render_widget(st, chunks[idx]);
        idx += 1;
    }

    // Navegador (com janela deslizante + âncora de seleção).
    let nav_area = chunks[idx];
    idx += 1;
    render_browser(f, nav_area, &v, scan_state, selected);
    if v.ssh_errors_open {
        render_ssh_errors_popup(f, nav_area, &v);
    }

    f.render_widget(
        Paragraph::new(footer_line(&v, scan_state))
            .block(Block::default().title("Controles").borders(Borders::ALL)),
        chunks[idx],
    );
}

/// Popup sobre o navegador: detalhes das falhas de conexão SSH (`d`).
fn render_ssh_errors_popup(f: &mut Frame, area: ratatui::layout::Rect, v: &UiView) {
    use ratatui::widgets::Clear;
    f.render_widget(Clear, area);
    let rows = ssh_error_rows(v);
    let title = if rows.is_empty() {
        "Falhas de conexão SSH — nenhuma".to_string()
    } else {
        format!("Falhas de conexão SSH ({}) — Esc/d fecha", rows.len())
    };
    let table = Table::new(rows, [Constraint::Length(22), Constraint::Min(10)])
        .header(
            Row::new(vec!["Host", "Motivo"]).style(
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Red)),
        );
    f.render_widget(table, area);
}

/// Navegador de pastas/arquivos com janela deslizante e cursor por destaque.
fn render_browser(
    f: &mut Frame,
    nav_area: ratatui::layout::Rect,
    v: &UiView,
    scan_state: (bool, usize, usize),
    selected: usize,
) {
    let nav_h = nav_area.height.saturating_sub(4) as usize;
    let total = v.items.len();
    let sel = selected.min(total.saturating_sub(1));
    let start = if total > nav_h.max(1) && sel + 2 >= nav_h.max(1) {
        (sel + 2)
            .saturating_sub(nav_h.max(1))
            .min(total.saturating_sub(nav_h.max(1)))
    } else {
        0
    };
    let end = (start + nav_h.max(1)).min(total);
    let max_phys = v.items.iter().map(|i| i.size_physical).max().unwrap_or(0);
    let slice: Vec<&ScanItem> = v.items[start..end].to_vec();
    let sub_view = UiView {
        title_dir: v.title_dir,
        snap: v.snap,
        items: slice,
        browser: v.browser,
        cfg: v.cfg,
        remotes_parents: v.remotes_parents,
        show_boot: v.show_boot,
        only_volumes: v.only_volumes,
        only_folders: v.only_folders,
        cloud_size: v.cloud_size,
        status: v.status,
        ssh_errors_open: v.ssh_errors_open,
    };
    let title = if v.browser.search_mode {
        format!(
            "Navegador: {} (Buscar: {}▏)",
            v.title_dir, v.browser.search_query
        )
    } else if scan_state.0 {
        format!(
            "Navegador: {} (Varrendo: {}/{})",
            v.title_dir, scan_state.1, scan_state.2
        )
    } else {
        format!("Navegador: {}", v.title_dir)
    };
    let nav_block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    if v.browser.show_help {
        let ht = Table::new(help_rows(), [Constraint::Length(15), Constraint::Min(10)]).block(
            Block::default()
                .title("Atalhos do Teclado - Ajuda")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow)),
        );
        f.render_widget(ht, nav_area);
    } else {
        let bt = Table::new(
            browser_rows(&sub_view, max_phys),
            [
                Constraint::Length(1),
                Constraint::Min(10),
                Constraint::Length(8),
                Constraint::Length(15),
                Constraint::Length(15),
                Constraint::Length(12),
            ],
        )
        .header(
            Row::new(vec![
                "",
                "Nome do Item",
                "Tipo",
                "Tamanho Físico",
                "Tamanho Lógico",
                "Proporção",
            ])
            .style(
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
        )
        .block(nav_block)
        .highlight_symbol(">")
        .row_highlight_style(
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        );
        let mut tstate = TableState::default();
        if !sub_view.items.is_empty() {
            tstate.select(Some(sel.saturating_sub(start)));
        }
        f.render_stateful_widget(bt, nav_area, &mut tstate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Browser, SortBy};
    use crate::config::AppConfig;
    use crate::scan::ScanItem;
    use crate::ssh::RemotePartition;
    use crate::system::{CacheDetail, DiskGroupView, SystemSnapshot};
    use ratatui::{backend::TestBackend, Terminal};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn test_cfg() -> AppConfig {
        AppConfig::default()
    }

    fn test_item(name: &str, is_dir: bool, phys: u64, log: u64, status: ItemStatus) -> ScanItem {
        ScanItem {
            name: name.to_string(),
            is_dir,
            path: PathBuf::from(name),
            size_logical: log,
            size_physical: phys,
            status,
        }
    }

    fn test_snap() -> SystemSnapshot {
        let mut usages = HashMap::new();
        usages.insert(
            "/".to_string(),
            crate::system::VolumeUsage {
                total: 100 << 30,
                used: 40 << 30,
                free: 60 << 30,
                fake: false,
            },
        );
        usages.insert(
            "/boot/efi".to_string(),
            crate::system::VolumeUsage {
                total: 1 << 30,
                used: 100 << 20,
                free: 900 << 20,
                fake: false,
            },
        );
        usages.insert(
            "/mnt/g".to_string(),
            crate::system::VolumeUsage {
                total: 0,
                used: 5 << 20,
                free: 0,
                fake: true,
            },
        );
        let mut mount_parents = HashMap::new();
        mount_parents.insert("/mnt/g".to_string(), "gdrive".to_string());
        let mut remote_hosts = HashMap::new();
        remote_hosts.insert(
            "h1".to_string(),
            vec![
                RemotePartition {
                    device: "/dev/sda1".to_string(),
                    fstype: String::new(),
                    size: 50 << 30,
                    used: 10 << 30,
                    free: 40 << 30,
                    used_pct: 20.0,
                    mountpoint: "/".to_string(),
                    is_remote: false,
                },
                RemotePartition {
                    device: "gdrive:".to_string(),
                    fstype: String::new(),
                    size: 0,
                    used: 0,
                    free: 0,
                    used_pct: 0.0,
                    mountpoint: "/mnt/g".to_string(),
                    is_remote: true,
                },
            ],
        );
        SystemSnapshot {
            ram_total: 8 << 30,
            ram_used: 2 << 30,
            ram_avail: 6 << 30,
            ram_pct: 25.0,
            swap_total: 0,
            swap_used: 0,
            swap_free: 0,
            swap_pct: 0.0,
            partitions: vec![
                crate::system::PartitionView {
                    device: "/dev/sda1".to_string(),
                    mountpoint: "/".to_string(),
                    fstype: "ext4".to_string(),
                    remote: false,
                },
                crate::system::PartitionView {
                    device: "/dev/sda1".to_string(),
                    mountpoint: "/boot/efi".to_string(),
                    fstype: "vfat".to_string(),
                    remote: false,
                },
                crate::system::PartitionView {
                    device: "gdrive:".to_string(),
                    mountpoint: "/mnt/g".to_string(),
                    fstype: "fuse.rclone".to_string(),
                    remote: true,
                },
            ],
            usages,
            disk_groups: Vec::new(),
            mount_parents,
            remotes_parent_count: [("gdrive".to_string(), 2)].into_iter().collect(),
            parent_totals: HashMap::new(),
            parent_used_sum: HashMap::new(),
            total_physical: 100 << 30,
            used_physical: 40 << 30,
            free_physical: 60 << 30,
            total_cloud: 0,
            used_cloud: 0,
            free_cloud: 0,
            total_virtual: 100 << 30,
            used_virtual: 40 << 30,
            free_virtual: 60 << 30,
            rclone_cache_total: 5 << 20,
            cache_details: vec![CacheDetail {
                name: "gdrive".to_string(),
                allocated: 5 << 20,
                apparent: 6 << 20,
            }],
            remote_hosts,
            ssh_errors: HashMap::new(),
        }
    }

    fn test_items() -> Vec<ScanItem> {
        vec![
            test_item("big", true, 90 << 20, 80 << 20, ItemStatus::Ok),
            test_item("small.txt", false, 10, 10, ItemStatus::Ok),
            test_item("wip", true, 0, 0, ItemStatus::Calculating),
            test_item("slow", true, 0, 0, ItemStatus::Timeout),
            test_item("bad", false, 0, 0, ItemStatus::Error),
        ]
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

    #[allow(clippy::too_many_arguments)] // NOSONAR: test adapter keeps each independent UI toggle explicit.
    fn draw_all(
        snap: &SystemSnapshot,
        items: &[&ScanItem],
        cfg: &AppConfig,
        browser: &Browser,
        show_boot: bool,
        only_volumes: bool,
        only_folders: bool,
        cloud_size: bool,
        status: Option<&str>,
    ) -> String {
        let backend = TestBackend::new(100, 40);
        let mut t = Terminal::new(backend).unwrap();
        t.draw(|f| {
            draw(
                f,
                UiView {
                    title_dir: "/tmp",
                    snap,
                    items: items.to_vec(),
                    browser,
                    cfg,
                    remotes_parents: &snap.mount_parents,
                    show_boot,
                    only_volumes,
                    only_folders,
                    cloud_size,
                    status,
                    ssh_errors_open: false,
                },
                (false, 5, 5),
                0,
            );
        })
        .unwrap();
        screen_text(&t)
    }

    fn browser() -> Browser {
        Browser::new(SortBy::Physical, true, String::new())
    }

    #[test]
    fn full_view_has_all_panels() {
        let cfg = test_cfg();
        let snap = test_snap();
        let items = test_items();
        let refs: Vec<&ScanItem> = items.iter().collect();
        let b = browser();
        let s = draw_all(&snap, &refs, &cfg, &b, false, false, false, false, None);
        for marker in [
            "disk-util-rs",
            "Memória",
            "Discos Locais",
            "Servidores Remotos",
            "Navegador",
            "FÍSICO",
            "big/",
            "small.txt",
            "calculando",
            "timeout",
            "erro",
            "Cache rclone",
            "Host: h1",
        ] {
            assert!(s.contains(marker), "faltando {marker}");
        }
    }

    #[test]
    fn boot_hidden_by_default_shown_on_flag() {
        let cfg = test_cfg();
        let snap = test_snap();
        let items = test_items();
        let refs: Vec<&ScanItem> = items.iter().collect();
        let b = browser();
        let hidden = draw_all(&snap, &refs, &cfg, &b, false, false, false, false, None);
        assert!(!hidden.contains("/boot/efi"));
        let shown = draw_all(&snap, &refs, &cfg, &b, true, false, false, false, None);
        assert!(shown.contains("/boot/efi"));
    }

    #[test]
    fn cloud_size_shows_cache_logical_for_fake() {
        let cfg = test_cfg();
        let snap = test_snap();
        let items = test_items();
        let refs: Vec<&ScanItem> = items.iter().collect();
        let b = browser();
        let plain = draw_all(&snap, &refs, &cfg, &b, false, false, false, false, None);
        assert!(plain.contains('—'));
        let cloud = draw_all(&snap, &refs, &cfg, &b, false, false, false, true, None);
        assert!(cloud.contains("6.0M") || cloud.contains('M'));
    }

    #[test]
    fn only_volumes_hides_browser() {
        let cfg = test_cfg();
        let snap = test_snap();
        let b = browser();
        let s = draw_all(&snap, &[], &cfg, &b, false, true, false, false, None);
        assert!(s.contains("Discos Locais"));
        assert!(!s.contains("Navegador"));
    }

    #[test]
    fn only_folders_hides_ram_and_volumes() {
        let cfg = test_cfg();
        let snap = test_snap();
        let items = test_items();
        let refs: Vec<&ScanItem> = items.iter().collect();
        let b = browser();
        let s = draw_all(&snap, &refs, &cfg, &b, false, false, true, false, None);
        assert!(s.contains("Navegador"));
        assert!(!s.contains("Memória"));
        assert!(!s.contains("Discos Locais"));
    }

    #[test]
    fn help_search_filter_and_status_modes() {
        let cfg = test_cfg();
        let snap = test_snap();
        let items = test_items();
        let refs: Vec<&ScanItem> = items.iter().collect();
        let mut b = browser();
        b.show_help = true;
        let s = draw_all(&snap, &refs, &cfg, &b, false, false, false, false, None);
        assert!(s.contains("Atalhos"));
        b.show_help = false;
        b.search_mode = true;
        b.search_query = "bi".to_string();
        let s = draw_all(&snap, &refs, &cfg, &b, false, false, false, false, None);
        assert!(s.contains("Buscar:"));
        b.search_mode = false;
        b.filter_mode = true;
        b.filter_query = "sm".to_string();
        let s = draw_all(
            &snap,
            &refs,
            &cfg,
            &b,
            false,
            false,
            false,
            false,
            Some("ok!"),
        );
        assert!(s.contains("Filtro:"));
        assert!(s.contains("ok!"));
    }

    #[test]
    fn disk_groups_render_one_row_per_disk() {
        let cfg = test_cfg();
        let mut snap = test_snap();
        snap.disk_groups = vec![
            DiskGroupView {
                label: "sda".into(),
                mountpoints: vec!["/".into(), "/boot/efi".into()],
                total: 101 << 30,
                used: 40 << 30,
                free: 61 << 30,
            },
            DiskGroupView {
                label: "lpool".into(),
                mountpoints: vec!["/dados".into()],
                total: 800 << 30,
                used: 450 << 30,
                free: 350 << 30,
            },
        ];
        let b = browser();
        let s = draw_all(&snap, &[], &cfg, &b, false, false, false, false, None);
        assert!(s.contains("sda ("), "linha do disco sda: {s}");
        assert!(s.contains("lpool ("), "linha do pool lpool: {s}");
        // Boot oculto: mount /boot/efi some do label do grupo.
        assert!(!s.contains("/boot/efi"));
        let shown = draw_all(&snap, &[], &cfg, &b, true, false, false, false, None);
        assert!(shown.contains("/boot/efi"));
    }

    #[test]
    fn ssh_failures_render_in_panel_and_footer() {
        let cfg = test_cfg();
        let mut snap = test_snap();
        snap.remote_hosts.clear();
        snap.ssh_errors
            .insert("down-host".into(), "timeout: sem resposta após 4s".into());
        let b = browser();
        let s = draw_all(&snap, &[], &cfg, &b, false, false, false, false, None);
        assert!(
            s.contains("Host: down-host — falhou"),
            "host morto no painel: {s}"
        );
        assert!(s.contains("[d] erros ssh"), "hint no footer: {s}");
        assert!(s.contains("Servidores Remotos"), "painel só com falhas");
    }

    #[test]
    fn ssh_errors_popup_lists_host_and_reason() {
        let cfg = test_cfg();
        let mut snap = test_snap();
        snap.remote_hosts.clear();
        snap.ssh_errors
            .insert("down-host".into(), "timeout: sem resposta após 4s".into());
        let b = browser();
        let backend = TestBackend::new(100, 40);
        let mut t = Terminal::new(backend).unwrap();
        t.draw(|f| {
            draw(
                f,
                UiView {
                    title_dir: "/x",
                    snap: &snap,
                    items: vec![],
                    browser: &b,
                    cfg: &cfg,
                    remotes_parents: &snap.mount_parents,
                    show_boot: false,
                    only_volumes: false,
                    only_folders: false,
                    cloud_size: false,
                    status: None,
                    ssh_errors_open: true,
                },
                (false, 0, 0),
                0,
            );
        })
        .unwrap();
        let s = screen_text(&t);
        assert!(s.contains("Falhas de conexão SSH"), "popup: {s}");
        assert!(s.contains("down-host"), "host no popup: {s}");
        assert!(s.contains("timeout: sem resposta"), "motivo no popup: {s}");
    }

    #[test]
    fn empty_items_and_no_ram_no_ssh() {
        let cfg = test_cfg();
        let mut snap = test_snap();
        snap.remote_hosts.clear();
        snap.ram_total = 0;
        snap.partitions.clear();
        snap.usages.clear();
        snap.total_physical = 0;
        snap.rclone_cache_total = 0;
        let b = browser();
        let s = draw_all(&snap, &[], &cfg, &b, false, false, false, false, None);
        assert!(s.contains("Pasta vazia"));
        assert!(s.contains("Desativada"));
        assert!(!s.contains("Servidores Remotos"));
    }

    #[test]
    fn decimal_format_and_scanning_title() {
        let mut cfg = test_cfg();
        cfg.display.byte_format = "decimal".to_string();
        let snap = test_snap();
        let items = test_items();
        let refs: Vec<&ScanItem> = items.iter().collect();
        let backend = TestBackend::new(100, 40);
        let mut t = Terminal::new(backend).unwrap();
        let b = browser();
        t.draw(|f| {
            draw(
                f,
                UiView {
                    title_dir: "/x",
                    snap: &snap,
                    items: refs.clone(),
                    browser: &b,
                    cfg: &cfg,
                    remotes_parents: &snap.mount_parents,
                    show_boot: false,
                    only_volumes: false,
                    only_folders: false,
                    cloud_size: false,
                    status: None,
                    ssh_errors_open: false,
                },
                (true, 1, 5),
                3,
            );
        })
        .unwrap();
        let s = screen_text(&t);
        assert!(s.contains("Varrendo: 1/5"));
        assert!(s.contains("M") || s.contains("G") || s.contains("K"));
    }

    #[test]
    fn utc_hms_shape() {
        let h = utc_hms();
        assert!(h.contains("UTC"));
        assert_eq!(h.len(), "00:00:00 UTC".len());
    }
}
