//! Walk nativo (jwalk + rayon): 1 walk = apparent + allocated.
//!
//! Substitui os forks `du -skx` do Python:
//! - apparent = `st_size`, allocated = `st_blocks * 512` (`MetadataExt`);
//! - hardlinks deduplicados via `DashSet<(dev, ino)>`;
//! - xdev: não cruza mount (`st_dev` != dev da raiz é podado);
//! - FUSE nunca é walkado (redirect p/ cache VFS feito em `scan.rs`);
//! - cancelamento via [`CancellationToken`]: aborta ao trocar de diretório.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashSet;
use jwalk::WalkDir;
use rayon::prelude::*;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkStatus {
    Ok,
    Timeout,
    Error,
}

#[derive(Debug, Clone, Copy)]
pub struct DirSizes {
    pub apparent: u64,
    pub allocated: u64,
    pub status: WalkStatus,
}

/// `true` se `path` está no mesmo device da raiz (xdev). Erro → `false`.
fn same_device(path: &Path, root_dev: u64) -> bool {
    use std::os::unix::fs::MetadataExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) => m.dev() == root_dev,
        Err(_) => false,
    }
}
/// (apparent, allocated, hardlinks vistos neste batch).
type Partial = (u64, u64, Vec<(u64, u64)>);

/// Soma (apparent, allocated) de `path` em UM walk. Arquivo único: stat direto.
pub fn dir_size(path: &Path, cancel: &CancellationToken) -> DirSizes {
    use std::os::unix::fs::MetadataExt;

    let root_meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => {
            return DirSizes {
                apparent: 0,
                allocated: 0,
                status: WalkStatus::Error,
            }
        }
    };
    if !root_meta.is_dir() {
        // Arquivo ou symlink: sem walk, sem dedup (top-level conta 1x).
        return DirSizes {
            apparent: root_meta.size(),
            allocated: root_meta.blocks() * 512,
            status: WalkStatus::Ok,
        };
    }
    let root_dev = root_meta.dev();
    let seen: DashSet<(u64, u64)> = DashSet::new();
    let apparent = AtomicU64::new(0);
    let allocated = AtomicU64::new(0);
    let cancelled = std::sync::atomic::AtomicBool::new(false);

    // Conta a raiz (como o `du` inclui a entrada do próprio diretório).
    apparent.fetch_add(root_meta.size(), Ordering::Relaxed);
    allocated.fetch_add(root_meta.blocks() * 512, Ordering::Relaxed);

    let walker = WalkDir::new(path)
        .skip_hidden(false)
        .follow_links(false)
        .process_read_dir(move |_depth, _path, _state, children| {
            children.retain(|entry| match entry {
                Ok(e) => same_device(&e.path(), root_dev),
                Err(_) => false,
            });
        });

    // Batches limitados: jwalk paraleliza o readdir, rayon paraleliza o stat.
    const BATCH: usize = 4096;
    let mut batch: Vec<std::path::PathBuf> = Vec::with_capacity(BATCH);
    let flush = |batch: &mut Vec<std::path::PathBuf>| {
        let paths = std::mem::replace(batch, Vec::with_capacity(BATCH));
        let partial: Vec<Partial> = paths
            .par_iter()
            .map(|p| {
                use std::os::unix::fs::MetadataExt;
                let Ok(m) = std::fs::symlink_metadata(p) else {
                    return (0, 0, Vec::new());
                };
                if m.dev() != root_dev {
                    return (0, 0, Vec::new());
                }
                let mut links = Vec::new();
                if m.nlink() > 1 && m.is_file() {
                    links.push((m.dev(), m.ino()));
                }
                (m.size(), m.blocks() * 512, links)
            })
            .collect();
        for (size, blocks, links) in partial {
            let mut skip = false;
            for key in links {
                if !seen.insert(key) {
                    skip = true;
                    break;
                }
            }
            if skip {
                continue;
            }
            apparent.fetch_add(size, Ordering::Relaxed);
            allocated.fetch_add(blocks, Ordering::Relaxed);
        }
    };

    for entry in walker {
        if cancel.is_cancelled() {
            cancelled.store(true, Ordering::Relaxed);
            break;
        }
        let Ok(e) = entry else { continue };
        // Symlinks contam pela própria entrada, sem descer (follow_links=false).
        batch.push(e.path());
        if batch.len() >= BATCH {
            flush(&mut batch);
            if cancel.is_cancelled() {
                cancelled.store(true, Ordering::Relaxed);
                break;
            }
        }
    }
    if !batch.is_empty() && !cancelled.load(Ordering::Relaxed) {
        flush(&mut batch);
    }

    DirSizes {
        apparent: apparent.load(Ordering::Relaxed),
        allocated: allocated.load(Ordering::Relaxed),
        status: if cancelled.load(Ordering::Relaxed) {
            WalkStatus::Timeout
        } else {
            WalkStatus::Ok
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn token() -> CancellationToken {
        CancellationToken::new()
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), vec![b'x'; 3000]).unwrap();
        std::fs::write(dir.path().join("sub").join("b.txt"), vec![b'y'; 100]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let _ = symlink(dir.path().join("a.txt"), dir.path().join("link.txt"));
        }
        let mut f = std::fs::File::create(dir.path().join("sub").join("c.bin")).unwrap();
        f.write_all(&[0u8; 512]).unwrap();
        dir
    }

    #[test]
    fn single_walk_returns_both_sizes() {
        let dir = fixture();
        let s = dir_size(dir.path(), &token());
        assert_eq!(s.status, WalkStatus::Ok);
        assert!(s.apparent >= 3000 + 100 + 512, "apparent={}", s.apparent);
        // Blocos alocados cobrem ao menos os bytes aparentes (arredondado p/ cima).
        assert!(
            s.allocated >= s.apparent,
            "alloc={} appar={}",
            s.allocated,
            s.apparent
        );
    }

    #[test]
    fn file_counts_once() {
        let dir = fixture();
        let s = dir_size(&dir.path().join("a.txt"), &token());
        assert_eq!(s.status, WalkStatus::Ok);
        assert_eq!(s.apparent, 3000);
    }

    #[test]
    fn hardlink_dedup_counts_once() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Payload de 1 MiB: overhead de entradas de diretório varia por
            // filesystem (tmpfs/ext4 relatam tamanhos de dir diferentes e o
            // runner derrubava a margem de 8 KiB). Com 1 MiB, dedup correto
            // fica ≈ SIZE + overhead; dedup quebrado daria ≈ 2×SIZE.
            const SIZE: usize = 1024 * 1024;
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("orig.bin"), vec![7u8; SIZE]).unwrap();
            std::fs::hard_link(dir.path().join("orig.bin"), dir.path().join("copy.bin")).unwrap();
            let meta = std::fs::metadata(dir.path().join("orig.bin")).unwrap();
            assert!(meta.nlink() >= 2);
            let s = dir_size(dir.path(), &token());
            assert_eq!(s.status, WalkStatus::Ok);
            assert!(s.apparent >= SIZE as u64);
            assert!(s.apparent < 2 * SIZE as u64, "apparent={}", s.apparent);
        }
    }

    #[test]
    fn missing_path_is_error() {
        let s = dir_size(Path::new("/nonexistent-disk-util-rs-xyz"), &token());
        assert_eq!(s.status, WalkStatus::Error);
        assert_eq!((s.apparent, s.allocated), (0, 0));
    }

    #[test]
    fn cancelled_walk_reports_timeout() {
        let cancel = token();
        cancel.cancel();
        let dir = fixture();
        let s = dir_size(dir.path(), &cancel);
        assert_eq!(s.status, WalkStatus::Timeout);
    }

    #[test]
    fn same_device_guards_xdev() {
        use std::os::unix::fs::MetadataExt;
        let root_dev = std::fs::metadata("/").expect("stat /").dev();
        // /proc é outro filesystem na prática; se não for, o teste ainda vale
        // como "mesmo device retorna true".
        let proc_dev = std::fs::symlink_metadata("/proc/version").map(|m| m.dev());
        if let Ok(pd) = proc_dev {
            assert_eq!(
                same_device(Path::new("/proc/version"), root_dev),
                pd == root_dev
            );
        }
        assert!(same_device(Path::new("/"), root_dev));
        assert!(!same_device(
            Path::new("/nonexistent-du-rs-samedev"),
            root_dev
        ));
    }
}
