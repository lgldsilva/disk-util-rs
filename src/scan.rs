//! Scanner de diretórios: 1 task por scan, `CancellationToken` por scan.
//!
//! Trocar de diretório cancela o token anterior (aborta walks em voo).
//! Redirect FUSE: dentro de mount rclone, mede o cache VFS local
//! (`rclone_cache_path`) em vez de walkar o FUSE — igual ao Python.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::mounts::Mount;
use crate::rclone::{find_remote_mount_info, rclone_cache_path};
use crate::walk::{dir_size, WalkStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ItemStatus {
    #[default]
    Calculating,
    Ok,
    Timeout,
    Error,
}

#[derive(Debug, Clone)]
pub struct ScanItem {
    pub name: String,
    pub is_dir: bool,
    pub path: PathBuf,
    pub size_logical: u64,
    pub size_physical: u64,
    pub status: ItemStatus,
}

#[derive(Debug, Default, Clone)]
pub struct ScanState {
    pub target_dir: PathBuf,
    pub items: Vec<ScanItem>,
    pub scanning: bool,
    pub scanned_count: usize,
    pub total_count: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScannerConfig {
    pub walk_item: Duration,
    pub walk_cache: Duration,
    pub cache_base: String,
    pub remote_markers: Vec<String>,
    pub mounts: Vec<Mount>,
}

#[derive(Debug)]
pub struct FolderScanner {
    state: Arc<Mutex<ScanState>>,
    cancel: Option<CancellationToken>,
    _handle: Option<tokio::task::JoinHandle<()>>,
}

impl FolderScanner {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ScanState::default())),
            cancel: None,
            _handle: None,
        }
    }

    pub fn state(&self) -> Arc<Mutex<ScanState>> {
        self.state.clone()
    }

    /// Cópia consistente do estado (p/ TUI e `--once`).
    pub fn snapshot(&self) -> ScanState {
        self.state.lock().expect("scan state").clone()
    }

    /// Inicia scan de `dir`; cancela o scan anterior.
    pub fn start_scan(&mut self, dir: PathBuf, cfg: ScannerConfig) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        {
            let mut st = self.state.lock().expect("scan state");
            st.target_dir = dir.clone();
            st.items.clear();
            st.scanning = true;
            st.scanned_count = 0;
            st.total_count = 0;
            st.error = None;
        }
        let state = self.state.clone();
        let handle = tokio::spawn(async move {
            scan_worker(dir, cfg, cancel, state).await;
        });
        self._handle = Some(handle);
    }

    pub fn cancel(&mut self) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
    }
}

impl Default for FolderScanner {
    fn default() -> Self {
        Self::new()
    }
}

async fn scan_worker(
    dir: PathBuf,
    cfg: ScannerConfig,
    cancel: CancellationToken,
    state: Arc<Mutex<ScanState>>,
) {
    // Lista top-level (sem descer em mounts: `os.path.ismount` do Python).
    let mut entries: Vec<(String, bool, PathBuf)> = Vec::new();
    let read = tokio::fs::read_dir(&dir).await;
    let mut rd = match read {
        Ok(rd) => rd,
        Err(e) => {
            let mut st = state.lock().expect("scan state");
            st.error = Some(e.to_string());
            st.scanning = false;
            return;
        }
    };
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let entry = match rd.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => {
                let mut st = state.lock().expect("scan state");
                st.error = Some(e.to_string());
                st.scanning = false;
                return;
            }
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let ft = match entry.file_type().await {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            entries.push((name, false, path));
            continue;
        }
        let is_dir = ft.is_dir();
        if is_dir {
            // Não lista/desce mountpoints (igual `os.path.ismount` do Python).
            if is_mountpoint(&path) {
                continue;
            }
            // xdev será reforçado pelo walk (st_dev), aqui só evita readdir.
        }
        entries.push((name, is_dir, path));
    }

    {
        let mut st = state.lock().expect("scan state");
        if cancel.is_cancelled() {
            return;
        }
        st.total_count = entries.len();
        st.items = entries
            .iter()
            .map(|(name, is_dir, path)| ScanItem {
                name: name.clone(),
                is_dir: *is_dir,
                path: path.clone(),
                size_logical: 0,
                size_physical: 0,
                status: ItemStatus::Calculating,
            })
            .collect();
    }

    let mount_info = find_remote_mount_info(&dir, &cfg.mounts, &cfg.remote_markers);

    // Paralelo limitado (semaphore 4) — Python forkava `du` em paralelo; serial = 20×3s.
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
    let mut set = tokio::task::JoinSet::new();
    for (idx, (name, is_dir, path)) in entries.iter().enumerate() {
        if cancel.is_cancelled() {
            return;
        }
        let _ = name;
        let permit = sem.clone().acquire_owned().await.ok();
        let path = path.clone();
        let is_dir = *is_dir;
        let mount_info = mount_info.clone();
        let cfg = cfg.clone();
        let cancel = cancel.clone();
        let state = state.clone();
        set.spawn(async move {
            let _permit = permit;
            let (logical, physical, status) = if is_dir {
                size_of_dir(&path, mount_info.as_ref(), &cfg, &cancel).await
            } else {
                size_of_file(&path, mount_info.as_ref(), &cfg)
            };
            let mut st = state.lock().expect("scan state");
            if cancel.is_cancelled() {
                return;
            }
            if let Some(item) = st.items.get_mut(idx) {
                item.size_logical = logical;
                item.size_physical = physical;
                item.status = status;
            }
            st.scanned_count += 1;
        });
    }
    while set.join_next().await.is_some() {
        if cancel.is_cancelled() {
            return;
        }
    }

    let mut st = state.lock().expect("scan state");
    if cancel.is_cancelled() {
        return;
    }
    st.items.sort_by_key(|a| std::cmp::Reverse(a.size_physical));
    st.scanning = false;
}

async fn size_of_dir(
    path: &Path,
    mount_info: Option<&(String, PathBuf, String, PathBuf)>,
    cfg: &ScannerConfig,
    cancel: &CancellationToken,
) -> (u64, u64, ItemStatus) {
    if let Some((device, mountpoint, _, _)) = mount_info {
        // Redirect FUSE: físico vem do cache VFS; lógico do cache apparent.
        let cache_path = rclone_cache_path(path, device, mountpoint, &cfg.cache_base);
        if !cache_path.exists() {
            return (0, 0, ItemStatus::Ok);
        }
        return walk_blocking(cache_path, cfg.walk_cache, cancel).await;
    }
    walk_blocking(path.to_path_buf(), cfg.walk_item, cancel).await
}

fn size_of_file(
    path: &Path,
    mount_info: Option<&(String, PathBuf, String, PathBuf)>,
    cfg: &ScannerConfig,
) -> (u64, u64, ItemStatus) {
    use std::os::unix::fs::MetadataExt;
    let (logical, mut physical) = match std::fs::symlink_metadata(path) {
        Ok(m) => (m.size(), m.blocks() * 512),
        Err(_) => return (0, 0, ItemStatus::Error),
    };
    if let Some((device, mountpoint, _, _)) = mount_info {
        let cache_path = rclone_cache_path(path, device, mountpoint, &cfg.cache_base);
        match std::fs::symlink_metadata(&cache_path) {
            Ok(m) => physical = m.blocks() * 512,
            Err(_) => physical = 0,
        }
        let _ = logical;
    }
    (logical, physical, ItemStatus::Ok)
}

async fn walk_blocking(
    path: PathBuf,
    timeout: Duration,
    cancel: &CancellationToken,
) -> (u64, u64, ItemStatus) {
    let child_cancel = cancel.clone();
    let kill = cancel.clone();
    let run = tokio::task::spawn_blocking(move || dir_size(&path, &child_cancel));
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok(s)) => (
            s.apparent,
            s.allocated,
            match s.status {
                WalkStatus::Ok => ItemStatus::Ok,
                WalkStatus::Timeout => ItemStatus::Timeout,
                WalkStatus::Error => ItemStatus::Error,
            },
        ),
        _ => {
            kill.cancel();
            (0, 0, ItemStatus::Timeout)
        }
    }
}

fn is_mountpoint(p: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return false;
    };
    match p.parent() {
        Some(parent) => match std::fs::symlink_metadata(parent) {
            Ok(pm) => pm.dev() != meta.dev(),
            Err(_) => true,
        },
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ScannerConfig {
        ScannerConfig {
            walk_item: Duration::from_secs(5),
            walk_cache: Duration::from_secs(2),
            cache_base: "/nonexistent-cache".to_string(),
            remote_markers: vec!["rclone".to_string()],
            mounts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn scans_top_level_and_sizes() {
        let dir = std::env::temp_dir().join(format!("scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("f.bin"), vec![1u8; 1024]).unwrap();
        std::fs::write(dir.join("top.txt"), vec![2u8; 512]).unwrap();

        let mut sc = FolderScanner::new();
        sc.start_scan(dir.clone(), cfg());
        // Aguarda concluir (poll simples, teste local rápido).
        for _ in 0..200 {
            {
                let st = sc.state.lock().unwrap();
                if !st.scanning && st.total_count > 0 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let st = sc.state.lock().unwrap();
        assert!(!st.scanning);
        assert_eq!(st.total_count, 2);
        let sub = st.items.iter().find(|i| i.name == "sub").unwrap();
        assert!(sub.size_logical >= 1024, "logical={}", sub.size_logical);
        assert_eq!(sub.status, ItemStatus::Ok);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rescan_cancels_previous() {
        let dir = std::env::temp_dir().join(format!("scan2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        let mut sc = FolderScanner::new();
        sc.start_scan(dir.join("a"), cfg());
        sc.start_scan(dir.join("b"), cfg());
        tokio::time::sleep(Duration::from_millis(300)).await;
        let st = sc.state.lock().unwrap();
        assert!(st.target_dir.ends_with("b"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::mounts::Mount;

    fn test_cfg(caps: &std::path::Path) -> ScannerConfig {
        ScannerConfig {
            walk_item: Duration::from_secs(5),
            walk_cache: Duration::from_secs(2),
            cache_base: caps.to_string_lossy().into_owned(),
            remote_markers: vec!["rclone".to_string()],
            mounts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn fuse_redirect_measures_cache_not_mount() {
        let base = std::env::temp_dir().join(format!("scan-fuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // Mount real com 1 MiB; cache com 1 KiB → prova que mede o cache.
        let mnt = base.join("mnt");
        std::fs::create_dir_all(mnt.join("sub")).unwrap();
        std::fs::write(mnt.join("sub").join("real.bin"), vec![1u8; 1 << 20]).unwrap();
        std::fs::write(mnt.join("top.txt"), vec![2u8; 1 << 20]).unwrap();
        let cache = base.join("cache");
        std::fs::create_dir_all(cache.join("gdrive").join("media").join("sub")).unwrap();
        std::fs::write(
            cache.join("gdrive").join("media").join("sub").join("c.bin"),
            vec![3u8; 1024],
        )
        .unwrap();
        std::fs::write(
            cache.join("gdrive").join("media").join("top.txt"),
            vec![4u8; 2048],
        )
        .unwrap();

        let mut mounts = vec![Mount {
            device: "gdrive:media".to_string(),
            mountpoint: mnt.to_string_lossy().into_owned(),
            fstype: "fuse.rclone".to_string(),
        }];
        let mut cfg = test_cfg(&cache);
        cfg.mounts.append(&mut mounts);
        let mut sc = FolderScanner::new();
        sc.start_scan(mnt.clone(), cfg);
        for _ in 0..200 {
            {
                let st = sc.snapshot();
                if !st.scanning && st.total_count > 0 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let st = sc.snapshot();
        assert!(!st.scanning);
        let sub = st
            .items
            .iter()
            .find(|i| i.name == "sub")
            .expect("sub listado");
        // Físico vem do cache (1 KiB), não do mount (1 MiB).
        assert!(
            sub.size_physical < (1 << 20),
            "physical={}",
            sub.size_physical
        );
        assert_eq!(sub.status, ItemStatus::Ok);
        let top = st
            .items
            .iter()
            .find(|i| i.name == "top.txt")
            .expect("top listado");
        assert!(top.size_physical < (1 << 20));
        assert!(top.size_logical >= 2048);
        // Cache ausente → (0,0,Ok): remove sub do cache e revarre.
        let _ = std::fs::remove_dir_all(cache.join("gdrive").join("media").join("sub"));
        sc.start_scan(mnt.clone(), test_cfg(&cache));
        sc.cancel(); // cobre cancel() explícito
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn missing_dir_sets_error() {
        let mut sc = FolderScanner::new();
        sc.start_scan(
            PathBuf::from("/nonexistent-du-rs-scan"),
            test_cfg(Path::new("/nonexistent-cache")),
        );
        for _ in 0..100 {
            {
                let st = sc.snapshot();
                if !st.scanning {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let st = sc.snapshot();
        assert!(!st.scanning);
        assert!(st.error.is_some());
    }
}
