//! High-throughput file I/O and shared worker pool.
//!
//! On Linux, bulk source reads use batched `io_uring` (`openat` -> `read` -> `close`,
//! with optional `IORING_SETUP_IOPOLL` + `O_DIRECT` when `SMELLA_IO_IOPOLL=1` is set
//! and supported by the underlying filesystem) with transparent fallback to parallel
//! multi-threaded reads when `io_uring_setup` is unavailable (e.g. restricted containers).
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rayon::prelude::*;

/// Shared Rayon worker pool sized to the host's hardware concurrency.
pub fn pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism().ok()?.get();
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .ok()
    })
    .as_ref()
}

/// Read all `rel_paths` under `root` into UTF-8 strings in index order.
pub fn read_files(root: &Path, rel_paths: &[String]) -> Vec<Result<String, String>> {
    if rel_paths.is_empty() {
        return Vec::new();
    }
    #[cfg(target_os = "linux")]
    if rel_paths.len() >= 32
        && let Some(results) = linux_uring::read_files_uring(root, rel_paths)
    {
        return results;
    }

    let read_one = |rel: &String| -> Result<String, String> {
        fs::read_to_string(root.join(rel)).map_err(|e| e.to_string())
    };
    if rel_paths.len() >= 32
        && let Some(pool) = pool()
    {
        pool.install(|| rel_paths.par_iter().map(read_one).collect())
    } else {
        rel_paths.iter().map(read_one).collect()
    }
}

/// Read raw bytes for a batch of absolute paths in parallel.
pub fn read_bytes_batch(paths: &[PathBuf]) -> Vec<io::Result<Vec<u8>>> {
    if paths.len() >= 16
        && let Some(pool) = pool()
    {
        pool.install(|| paths.par_iter().map(fs::read).collect())
    } else {
        paths.iter().map(fs::read).collect()
    }
}

#[cfg(target_os = "linux")]
mod linux_uring {
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use io_uring::{IoUring, opcode, types};

    const BATCH: usize = 128;
    const INITIAL_READ_CAP: usize = 16 * 1024;
    const DIRECT_ALIGN: usize = 4096;

    /// Optional `IORING_SETUP_IOPOLL` + `O_DIRECT` probe for NVMe/block-backed filesystems.
    /// Falls back cleanly when `O_DIRECT` or `IOPOLL` is rejected (e.g. tmpfs/overlayfs).
    fn try_read_iopoll_direct(
        root: &Path,
        rel_paths: &[String],
    ) -> Option<Vec<Result<String, String>>> {
        let mut ring = IoUring::builder()
            .setup_iopoll()
            .build(BATCH as u32 * 2)
            .ok()?;
        let mut out: Vec<Result<String, String>> = vec![Err(String::new()); rel_paths.len()];

        for (chunk_idx, chunk) in rel_paths.chunks(BATCH).enumerate() {
            let base = chunk_idx * BATCH;
            let mut fds: Vec<i32> = Vec::with_capacity(chunk.len());
            let mut sizes: Vec<usize> = Vec::with_capacity(chunk.len());
            let mut bufs: Vec<Vec<u8>> = Vec::with_capacity(chunk.len());
            let mut aligned_ptrs: Vec<(*mut u8, usize)> = Vec::with_capacity(chunk.len());

            for (i, rel) in chunk.iter().enumerate() {
                let abs = root.join(rel);
                let Ok(cpath) = CString::new(abs.as_os_str().as_bytes()) else {
                    out[base + i] = fs::read_to_string(&abs).map_err(|e| e.to_string());
                    fds.push(-1);
                    sizes.push(0);
                    bufs.push(Vec::new());
                    aligned_ptrs.push((std::ptr::null_mut(), 0));
                    continue;
                };
                // SAFETY: `cpath` is NUL-terminated; flags are valid Linux open(2) flags.
                let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECT) };
                if fd < 0 {
                    // Filesystem does not support O_DIRECT (e.g. tmpfs/overlayfs) — bail to buffered io_uring.
                    for open_fd in fds {
                        if open_fd >= 0 {
                            unsafe { libc::close(open_fd) };
                        }
                    }
                    return None;
                }
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe { libc::fstat(fd, &mut st) } < 0 || st.st_size < 0 {
                    unsafe { libc::close(fd) };
                    out[base + i] = fs::read_to_string(&abs).map_err(|e| e.to_string());
                    fds.push(-1);
                    sizes.push(0);
                    bufs.push(Vec::new());
                    aligned_ptrs.push((std::ptr::null_mut(), 0));
                    continue;
                }
                let file_len = st.st_size as usize;
                if file_len == 0 {
                    unsafe { libc::close(fd) };
                    out[base + i] = Ok(String::new());
                    fds.push(-1);
                    sizes.push(0);
                    bufs.push(Vec::new());
                    aligned_ptrs.push((std::ptr::null_mut(), 0));
                    continue;
                }
                let aligned_len = (file_len + DIRECT_ALIGN - 1) & !(DIRECT_ALIGN - 1);
                let mut backing = vec![0u8; aligned_len + DIRECT_ALIGN];
                let raw_ptr = backing.as_mut_ptr() as usize;
                let offset = (DIRECT_ALIGN - (raw_ptr % DIRECT_ALIGN)) % DIRECT_ALIGN;
                let ptr = unsafe { backing.as_mut_ptr().add(offset) };
                fds.push(fd);
                sizes.push(file_len);
                bufs.push(backing);
                aligned_ptrs.push((ptr, aligned_len));
            }

            let mut submitted = 0usize;
            {
                let mut sq = ring.submission();
                for i in 0..chunk.len() {
                    if fds[i] < 0 {
                        continue;
                    }
                    let (ptr, aligned_len) = aligned_ptrs[i];
                    let entry = opcode::Read::new(types::Fd(fds[i]), ptr, aligned_len as u32)
                        .offset(0)
                        .build()
                        .user_data(i as u64);
                    // SAFETY: buffers live in `bufs` until completions drain below.
                    if unsafe { sq.push(&entry) }.is_ok() {
                        submitted += 1;
                    }
                }
            }
            if submitted > 0 && ring.submit_and_wait(submitted).is_err() {
                for open_fd in fds {
                    if open_fd >= 0 {
                        unsafe { libc::close(open_fd) };
                    }
                }
                return None;
            }
            let cq = ring.completion();
            for cqe in cq {
                let idx = cqe.user_data() as usize;
                let res = cqe.result();
                if fds[idx] >= 0 {
                    unsafe { libc::close(fds[idx]) };
                    fds[idx] = -1;
                }
                if res >= 0 {
                    let read_len = (res as usize).min(sizes[idx]);
                    let (ptr, _) = aligned_ptrs[idx];
                    // SAFETY: `ptr..ptr+read_len` lies within `bufs[idx]` and was initialized by kernel.
                    let slice = unsafe { std::slice::from_raw_parts(ptr, read_len) };
                    out[base + idx] = std::str::from_utf8(slice)
                        .map(str::to_owned)
                        .map_err(|e| e.to_string());
                } else {
                    let abs = root.join(&chunk[idx]);
                    out[base + idx] = fs::read_to_string(&abs).map_err(|e| e.to_string());
                }
            }
            for open_fd in fds {
                if open_fd >= 0 {
                    unsafe { libc::close(open_fd) };
                }
            }
        }
        Some(out)
    }

    /// Batched buffered `io_uring` pipeline: submits `OPENAT`, `READ`, and `CLOSE` in rings
    /// of `BATCH` entries to minimize syscall overhead across thousands of small source files.
    pub fn read_files_uring(
        root: &Path,
        rel_paths: &[String],
    ) -> Option<Vec<Result<String, String>>> {
        if std::env::var_os("SMELLA_IO_IOPOLL").is_some()
            && let Some(res) = try_read_iopoll_direct(root, rel_paths)
        {
            return Some(res);
        }

        let mut ring = IoUring::new((BATCH * 2) as u32).ok()?;
        let mut out: Vec<Result<String, String>> = vec![Err(String::new()); rel_paths.len()];

        for (chunk_idx, chunk) in rel_paths.chunks(BATCH).enumerate() {
            let base = chunk_idx * BATCH;
            let cpaths: Vec<Option<CString>> = chunk
                .iter()
                .map(|rel| CString::new(root.join(rel).as_os_str().as_bytes()).ok())
                .collect();

            // Phase 1: batched OPENAT.
            let mut fds = vec![-1i32; chunk.len()];
            let mut open_submitted = 0usize;
            {
                let mut sq = ring.submission();
                for (i, cp) in cpaths.iter().enumerate() {
                    if let Some(cp) = cp {
                        let entry = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), cp.as_ptr())
                            .flags(libc::O_RDONLY | libc::O_CLOEXEC)
                            .build()
                            .user_data(i as u64);
                        if unsafe { sq.push(&entry) }.is_ok() {
                            open_submitted += 1;
                        }
                    }
                }
            }
            if open_submitted > 0 {
                if ring.submit_and_wait(open_submitted).is_err() {
                    return None;
                }
                for cqe in ring.completion() {
                    let i = cqe.user_data() as usize;
                    let res = cqe.result();
                    if res >= 0 {
                        fds[i] = res;
                    } else {
                        out[base + i] = Err(std::io::Error::from_raw_os_error(-res).to_string());
                    }
                }
            }

            // Phase 2: batched READ (first 16 KiB covers >95% of JS/TS files in a single SQE).
            let mut bufs: Vec<Vec<u8>> = (0..chunk.len())
                .map(|i| {
                    if fds[i] >= 0 {
                        vec![0u8; INITIAL_READ_CAP]
                    } else {
                        Vec::new()
                    }
                })
                .collect();
            let mut read_submitted = 0usize;
            {
                let mut sq = ring.submission();
                for i in 0..chunk.len() {
                    if fds[i] < 0 {
                        continue;
                    }
                    let entry = opcode::Read::new(
                        types::Fd(fds[i]),
                        bufs[i].as_mut_ptr(),
                        bufs[i].len() as u32,
                    )
                    .offset(0)
                    .build()
                    .user_data(i as u64);
                    if unsafe { sq.push(&entry) }.is_ok() {
                        read_submitted += 1;
                    }
                }
            }
            let mut need_more: Vec<(usize, usize)> = Vec::new();
            if read_submitted > 0 {
                if ring.submit_and_wait(read_submitted).is_err() {
                    for fd in fds {
                        if fd >= 0 {
                            unsafe { libc::close(fd) };
                        }
                    }
                    return None;
                }
                for cqe in ring.completion() {
                    let i = cqe.user_data() as usize;
                    let res = cqe.result();
                    if res < 0 {
                        out[base + i] = Err(std::io::Error::from_raw_os_error(-res).to_string());
                    } else {
                        let n = res as usize;
                        if n < bufs[i].len() {
                            bufs[i].truncate(n);
                            out[base + i] = String::from_utf8(std::mem::take(&mut bufs[i]))
                                .map_err(|e| e.to_string());
                        } else {
                            need_more.push((i, n));
                        }
                    }
                }
            }

            // Finish any files larger than 16 KiB via standard pread loop on the already-open fd.
            for (i, mut off) in need_more {
                let mut extra = vec![0u8; 32 * 1024];
                loop {
                    let n = unsafe {
                        libc::pread(
                            fds[i],
                            extra.as_mut_ptr().cast(),
                            extra.len(),
                            off as libc::off_t,
                        )
                    };
                    if n <= 0 {
                        if n < 0 {
                            out[base + i] = Err(std::io::Error::last_os_error().to_string());
                        } else {
                            out[base + i] = String::from_utf8(std::mem::take(&mut bufs[i]))
                                .map_err(|e| e.to_string());
                        }
                        break;
                    }
                    let n = n as usize;
                    bufs[i].extend_from_slice(&extra[..n]);
                    off += n;
                }
            }

            // Phase 3: batched CLOSE.
            let mut close_submitted = 0usize;
            {
                let mut sq = ring.submission();
                for fd in &mut fds {
                    if *fd >= 0 {
                        let entry = opcode::Close::new(types::Fd(*fd)).build();
                        if unsafe { sq.push(&entry) }.is_ok() {
                            close_submitted += 1;
                            *fd = -1;
                        }
                    }
                }
            }
            if close_submitted > 0 {
                let _ = ring.submit_and_wait(close_submitted);
                let _ = ring.completion().count();
            }
            for fd in fds {
                if fd >= 0 {
                    unsafe { libc::close(fd) };
                }
            }
        }
        Some(out)
    }
}
