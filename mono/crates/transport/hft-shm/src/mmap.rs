//! SHM 파일 생성 / 매핑 / advise 래퍼.
//!
//! ## 세 가지 backing 모드
//!
//! 1. **Owned (tmpfs/hugetlbfs)** — [`ShmRegion::create_or_attach`] /
//!    [`ShmRegion::attach_existing`]. `/dev/shm/<name>` 또는 hugetlbfs 에 파일을
//!    만들고 `MAP_SHARED` 로 매핑. 단일 호스트 intra-OS IPC 용.
//! 2. **Device (ivshmem PCI BAR)** — [`ShmRegion::open_device`]. guest VM 안에서
//!    `/sys/bus/pci/devices/.../resource2` 를 열고 파일 전체를 RW 로 매핑. 파일
//!    크기는 BAR 크기로 고정 — `ftruncate` 나 `MADV_HUGEPAGE` 는 쓰지 않는다.
//! 3. **View (sub-region)** — [`ShmRegion::sub_view`]. 상위 [`ShmRegion`] 위에
//!    `offset/len` 범위만 바라보는 얇은 핸들. 내부적으로 `Arc<ShmRegion>` 으로
//!    parent 수명을 잡고, 자신이 보는 슬라이스 포인터를 그대로 노출한다. 매핑
//!    자체를 다시 만들지 않으므로 0-cost 복제.
//!
//! ## 공통 동작
//! - Owned 는 `MAP_SHARED` + `MAP_POPULATE`(Linux) + `MADV_WILLNEED/HUGEPAGE/DONTFORK`
//!   + best-effort `mlock`.
//! - Device 는 `MAP_SHARED` + `MAP_POPULATE`(지원 시). hugepage advise 없음
//!   (BAR 는 이미 hugepage-backed). `mlock` 은 생략 (PCI BAR 는 swap 대상 아님).
//! - View 는 parent 가 이미 처리.
//!
//! ## 공개 API
//! - `as_ptr` / `raw_base` / `as_mut_ptr` / `len` / `path` — 어떤 backing 이든
//!   일관된 포인터와 길이를 돌려준다. 상위 (quote_slot, rings, symbol_table,
//!   region) 는 backing 종류를 전혀 몰라도 동작.

use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::{MmapMut, MmapOptions};
use tracing::{debug, warn};

use crate::error::{ShmError, ShmResult};

/// mmap 전 **SIGBUS guard**. tmpfs/hugetlbfs backing 은 write 시점에 페이지를
/// 할당하며, 현재 fd 가 요구 크기보다 작으면 mmap 으로 확보한 뒤 접근시
/// SIGBUS 로 프로세스가 죽는다. 여기서 미리 `fstat` 해 `required` 바이트 이상
/// 보장됨을 확인.
fn check_fd_size_covers(file: &File, required: usize, path: &Path) -> ShmResult<()> {
    let meta = file
        .metadata()
        .map_err(|e| ShmError::path(path.to_path_buf(), e))?;
    let size = meta.len() as usize;
    if size < required {
        return Err(ShmError::Other(format!(
            "refusing to mmap — fd size {size} < required {required} at {:?} (would SIGBUS on access)",
            path
        )));
    }
    Ok(())
}

/// Linux 에서 `posix_fallocate(fd, 0, size)` 로 실제 블록 예약을 시도해
/// tmpfs ENOSPC 를 eager 하게 감지한다. 실패해도 fallback 으로 `set_len` 은
/// 이미 호출된 상태이므로 warn 로그만 남긴다 (hugetlbfs 는 fallocate 미지원).
#[cfg(target_os = "linux")]
fn try_posix_fallocate(file: &File, size: u64, path: &Path) -> ShmResult<()> {
    // SAFETY: fd 는 방금 열린 유효 핸들.
    let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, size as libc::off_t) };
    if rc == 0 {
        return Ok(());
    }
    // EOPNOTSUPP/EINVAL 은 hugetlbfs 등에서 기대되는 실패 — 치명적 아님.
    // ENOSPC 는 실제 디스크 부족 → 하드 실패로 승격.
    if rc == libc::ENOSPC {
        return Err(ShmError::Other(format!(
            "posix_fallocate ENOSPC on {:?} (size={})",
            path, size
        )));
    }
    debug!(
        errno = rc,
        path = %path.display(),
        size,
        "posix_fallocate unsupported (expected on hugetlbfs) — falling back to ftruncate only"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn try_posix_fallocate(_file: &File, _size: u64, _path: &Path) -> ShmResult<()> {
    Ok(())
}

/// 한 SHM 영역 — 파일 + mmap + 서브뷰의 수명 주기를 포괄.
///
/// `Drop` 시 Owned backing 은 mmap 을 자동 해제하지만 파일은 남는다 (다음
/// 프로세스가 재사용). 파일까지 지우려면 [`ShmRegion::unlink`] 를 명시적으로
/// 호출. View / Device backing 은 unlink 대상이 아님 (device 는 애초에 disk
/// file 이 아니고, view 는 parent 소유).
pub struct ShmRegion {
    /// 경로 — Owned 는 `/dev/shm/..`, Device 는 `/sys/.../resource2`, View 는 parent 경로 그대로.
    path: PathBuf,
    /// 실제 backing.
    kind: RegionKind,
}

/// 내부 backing 종류. 외부에는 노출되지 않음.
enum RegionKind {
    /// 자신이 열고 ftruncate 한 파일 + 그 파일의 mmap.
    Owned { _file: File, mmap: MmapMut },
    /// guest VM 의 ivshmem PCI BAR (`/sys/.../resource2`) 를 그대로 mmap.
    Device { _file: File, mmap: MmapMut },
    /// 상위 region 의 일부를 가리키는 뷰 — 자체 mmap 없음.
    View {
        parent: Arc<ShmRegion>,
        offset: usize,
        len: usize,
    },
}

impl ShmRegion {
    // ───────────────────────────── Owned (tmpfs/hugetlbfs) ──────────────────────

    /// 고정 `size` 바이트의 SHM 영역을 `create_or_attach` 모드로 연다.
    ///
    /// - 파일이 없으면 새로 생성하고 `ftruncate` 로 size 만큼 확장.
    /// - 이미 있으면 크기가 `size` 와 같은지 검증. 다르면 에러.
    ///
    /// `pre_populate` 이 true 면 Linux `MAP_POPULATE` (+ huge page 힌트) 를 시도.
    pub fn create_or_attach(path: &Path, size: usize, pre_populate: bool) -> ShmResult<Self> {
        if size == 0 {
            return Err(ShmError::Other("size must be > 0".into()));
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                return Err(ShmError::path(
                    parent.to_path_buf(),
                    std::io::Error::new(std::io::ErrorKind::NotFound, "parent dir missing"),
                ));
            }
        }

        let mut oo = OpenOptions::new();
        oo.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            oo.mode(0o660);
        }
        let file = oo
            .open(path)
            .map_err(|e| ShmError::path(path.to_path_buf(), e))?;

        // 기존 파일이면 size 검증, 없으면 새로 확장.
        let meta = file
            .metadata()
            .map_err(|e| ShmError::path(path.to_path_buf(), e))?;
        let cur_len = meta.len() as usize;
        if cur_len == 0 {
            file.set_len(size as u64)
                .map_err(|e| ShmError::path(path.to_path_buf(), e))?;
            let mut perms = meta.permissions();
            perms.set_mode(0o660);
            let _ = file.set_permissions(perms);
            // eagerly 블록 예약 — tmpfs ENOSPC 는 여기서 바로 실패. hugetlbfs 는
            // ENOTSUP 를 돌려주므로 조용히 무시.
            try_posix_fallocate(&file, size as u64, path)?;
        } else if cur_len != size {
            return Err(ShmError::Other(format!(
                "existing SHM at {:?} has size {} but expected {}",
                path, cur_len, size
            )));
        }

        // SIGBUS guard: truncate 후에도 반드시 size 이상인지 fstat 으로 재확인.
        check_fd_size_covers(&file, size, path)?;

        let mut opts = MmapOptions::new();
        opts.len(size);
        #[cfg(target_os = "linux")]
        if pre_populate {
            opts.populate();
        }
        #[cfg(not(target_os = "linux"))]
        let _ = pre_populate;

        // SAFETY: 방금 연 파일이며 size 바이트가 보장된다 (check_fd_size_covers).
        // mmap 은 file 의 유일한 backing 이고 `_file` 과 같은 구조체 안에서 같이
        // 수명이 관리된다.
        let mmap = unsafe { opts.map_mut(&file) }.map_err(ShmError::Io)?;

        let region = Self {
            path: path.to_path_buf(),
            kind: RegionKind::Owned { _file: file, mmap },
        };
        region.advise_owned()?;
        region.try_mlock_owned();
        debug!(path = %path.display(), size = size, "hft-shm owned region attached");
        Ok(region)
    }

    /// 기존 SHM 영역을 read+write 매핑으로 연다. 크기는 파일 실제 크기 그대로.
    pub fn attach_existing(path: &Path) -> ShmResult<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| ShmError::path(path.to_path_buf(), e))?;
        let size = file
            .metadata()
            .map_err(|e| ShmError::path(path.to_path_buf(), e))?
            .len() as usize;
        if size == 0 {
            return Err(ShmError::Other(format!(
                "existing SHM at {:?} is empty",
                path
            )));
        }
        // SIGBUS guard — 파일이 race 로 truncate 됐으면 여기서 걸러내자.
        check_fd_size_covers(&file, size, path)?;
        let mut opts = MmapOptions::new();
        opts.len(size);
        #[cfg(target_os = "linux")]
        opts.populate();
        // SAFETY: 동일 invariants. file 은 구조체 수명 내 유지.
        let mmap = unsafe { opts.map_mut(&file) }.map_err(ShmError::Io)?;
        let region = Self {
            path: path.to_path_buf(),
            kind: RegionKind::Owned { _file: file, mmap },
        };
        region.advise_owned()?;
        region.try_mlock_owned();
        Ok(region)
    }

    // ───────────────────────────── Device (ivshmem PCI BAR) ─────────────────────

    /// guest VM 안에서 ivshmem PCI BAR 를 통째로 매핑.
    ///
    /// 경로는 보통 `/sys/bus/pci/devices/<bdf>/resource2` (BAR2). 파일 크기는
    /// BAR 크기로 **고정** — `ftruncate` 안 함. `MADV_HUGEPAGE` 도 쓰지 않는다
    /// (BAR backing 은 이미 host 의 hugetlbfs). `mlock` 은 생략 (BAR 는 swap
    /// 안 됨).
    ///
    /// 반환된 region 은 이후 [`SharedRegion`] 쪽에서 header digest 를 읽어
    /// wire 호환성을 재검증해야 한다 — 여기선 단순 매핑만 한다.
    pub fn open_device(path: &Path) -> ShmResult<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| ShmError::path(path.to_path_buf(), e))?;
        let size = file
            .metadata()
            .map_err(|e| ShmError::path(path.to_path_buf(), e))?
            .len() as usize;
        if size == 0 {
            return Err(ShmError::Other(format!(
                "device SHM at {:?} reports zero size (BAR not exposed?)",
                path
            )));
        }
        // SIGBUS guard — BAR 는 실시간 크기 변동 없지만 재확인은 저렴하고 unmap 상태
        // 에서의 race 를 걸러낸다.
        check_fd_size_covers(&file, size, path)?;
        let mut opts = MmapOptions::new();
        opts.len(size);
        #[cfg(target_os = "linux")]
        opts.populate();
        // SAFETY: PCI BAR 는 kernel 이 관리하는 안정적 영역. file 핸들이 살아
        // 있는 한 매핑은 유효.
        let mmap = unsafe { opts.map_mut(&file) }.map_err(ShmError::Io)?;

        let region = Self {
            path: path.to_path_buf(),
            kind: RegionKind::Device { _file: file, mmap },
        };
        // hugepage advise 는 의미 없음. DONTFORK 는 fork 후 포인터 유지에 유용 →
        // best-effort 로만.
        region.advise_device_dontfork();
        debug!(path = %path.display(), size = size, "hft-shm device region attached");
        Ok(region)
    }

    // ───────────────────────────── Sub-view ─────────────────────────────────────

    /// 상위 region 의 `[offset, offset+len)` 구간을 얇은 뷰로 감싼다.
    ///
    /// 주의사항:
    /// - view 는 parent 의 mmap 을 다시 만들지 않는다. 동일 pointer arithmetic.
    /// - parent 는 `Arc` 로 공유되므로 view 가 살아있는 동안 소멸하지 않는다.
    /// - view 의 경로는 parent 와 동일 (파일 시스템 상 단일 파일이므로).
    /// - `offset` / `len` 은 parent.len() 범위 내여야 한다.
    pub fn sub_view(parent: Arc<ShmRegion>, offset: usize, len: usize) -> ShmResult<Self> {
        let parent_len = parent.len();
        let end = offset.checked_add(len).ok_or_else(|| {
            ShmError::Other(format!(
                "sub_view overflow: offset={} len={}",
                offset, len
            ))
        })?;
        if end > parent_len {
            return Err(ShmError::Other(format!(
                "sub_view out of range: offset={} len={} parent_len={}",
                offset, len, parent_len
            )));
        }
        if len == 0 {
            return Err(ShmError::Other("sub_view len must be > 0".into()));
        }
        let path = parent.path.clone();
        Ok(Self {
            path,
            kind: RegionKind::View {
                parent,
                offset,
                len,
            },
        })
    }

    // ───────────────────────────── 공통 accessor ────────────────────────────────

    /// raw const pointer — 읽기 전용 접근 진입점.
    pub fn as_ptr(&self) -> *const u8 {
        match &self.kind {
            RegionKind::Owned { mmap, .. } | RegionKind::Device { mmap, .. } => mmap.as_ptr(),
            RegionKind::View {
                parent, offset, ..
            } => {
                // SAFETY: offset 은 sub_view 시 parent.len 내에 있음이 검증됐다.
                unsafe { parent.as_ptr().add(*offset) }
            }
        }
    }

    /// raw mut pointer — 쓰기 접근 진입점 (호출자가 동기화 책임).
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        match &mut self.kind {
            RegionKind::Owned { mmap, .. } | RegionKind::Device { mmap, .. } => mmap.as_mut_ptr(),
            RegionKind::View {
                parent, offset, ..
            } => {
                // SAFETY: parent 의 mmap 포인터는 고정이며, offset 은 검증됐다.
                // 복수 view 가 겹치면 상위 (region.rs) 가 분리를 책임진다.
                unsafe { parent.raw_base().add(*offset) }
            }
        }
    }

    /// mmap 주소를 **immutable** 하게 꺼낸다. 내부적으로 atomic 접근을 사용하는
    /// 모듈이 `&self` 만 들고도 쓸 수 있게 하기 위한 escape hatch.
    ///
    /// # 안전
    /// 호출자는 이 포인터를 통해 접근하는 영역에 대해 race-free 하게 atomic
    /// 접근(`AtomicU64` / `AtomicU32`) 만 사용해야 한다. 일반 write/read 는 unsafe.
    pub fn raw_base(&self) -> *mut u8 {
        match &self.kind {
            RegionKind::Owned { mmap, .. } | RegionKind::Device { mmap, .. } => {
                mmap.as_ptr() as *mut u8
            }
            RegionKind::View {
                parent, offset, ..
            } => {
                // SAFETY: offset 검증된 값. parent 자체가 atomic 접근 안전.
                unsafe { parent.raw_base().add(*offset) }
            }
        }
    }

    /// mmap 된 바이트 크기. View 면 sub-range 크기, 그 외엔 전체.
    pub fn len(&self) -> usize {
        match &self.kind {
            RegionKind::Owned { mmap, .. } | RegionKind::Device { mmap, .. } => mmap.len(),
            RegionKind::View { len, .. } => *len,
        }
    }

    /// len == 0 여부 (거의 항상 false, 0 size 는 생성 단계에서 차단).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 파일 경로. View 는 parent 경로를 공유한다.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Owned / Device / View 인지. 테스트 및 진단용.
    pub fn is_view(&self) -> bool {
        matches!(self.kind, RegionKind::View { .. })
    }

    /// `unlink(2)` — 파일 제거. 이미 매핑된 프로세스들은 계속 쓸 수 있음 (POSIX).
    ///
    /// View / Device 에 호출하면 `Unsupported` — device 는 unlink 대상이 아니고
    /// view 는 parent 소유.
    pub fn unlink(&self) -> ShmResult<()> {
        match &self.kind {
            RegionKind::Owned { .. } => std::fs::remove_file(&self.path)
                .map_err(|e| ShmError::path(self.path.clone(), e)),
            RegionKind::Device { .. } => Err(ShmError::Unsupported(
                "cannot unlink device-backed region (PCI BAR)",
            )),
            RegionKind::View { .. } => Err(ShmError::Unsupported(
                "cannot unlink sub-view (no file ownership)",
            )),
        }
    }

    // ───────────────────────────── Linux advise / mlock ─────────────────────────

    #[cfg(target_os = "linux")]
    fn advise_owned(&self) -> ShmResult<()> {
        let RegionKind::Owned { mmap, .. } = &self.kind else {
            return Ok(());
        };
        let ptr = mmap.as_ptr() as *mut libc::c_void;
        let len = mmap.len();
        // SAFETY: ptr/len 은 우리가 mmap 한 유효 영역. madvise 는 실패해도 메모리
        // 안전에 영향 없음.
        unsafe {
            if libc::madvise(ptr, len, libc::MADV_WILLNEED) != 0 {
                warn!(
                    err = %std::io::Error::last_os_error(),
                    "madvise(MADV_WILLNEED) failed (non-fatal)"
                );
            }
            if libc::madvise(ptr, len, libc::MADV_HUGEPAGE) != 0 {
                debug!(
                    err = %std::io::Error::last_os_error(),
                    "madvise(MADV_HUGEPAGE) failed (expected if THP disabled or hugetlbfs-backed)"
                );
            }
            if libc::madvise(ptr, len, libc::MADV_DONTFORK) != 0 {
                warn!(
                    err = %std::io::Error::last_os_error(),
                    "madvise(MADV_DONTFORK) failed (non-fatal)"
                );
            }
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn advise_owned(&self) -> ShmResult<()> {
        Ok(())
    }

    /// PCI BAR 전용 — hugepage 요청은 커널이 거부하므로 생략, DONTFORK 만.
    #[cfg(target_os = "linux")]
    fn advise_device_dontfork(&self) {
        let RegionKind::Device { mmap, .. } = &self.kind else {
            return;
        };
        let ptr = mmap.as_ptr() as *mut libc::c_void;
        let len = mmap.len();
        // SAFETY: 유효 매핑.
        unsafe {
            if libc::madvise(ptr, len, libc::MADV_DONTFORK) != 0 {
                warn!(
                    err = %std::io::Error::last_os_error(),
                    "madvise(MADV_DONTFORK) on device region failed (non-fatal)"
                );
            }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn advise_device_dontfork(&self) {}

    /// best-effort `mlock`. 실패 시 `warn` 로그 후 계속.
    #[cfg(target_os = "linux")]
    fn try_mlock_owned(&self) {
        let RegionKind::Owned { mmap, .. } = &self.kind else {
            return;
        };
        let ptr = mmap.as_ptr() as *const libc::c_void;
        let len = mmap.len();
        // SAFETY: 유효 매핑 영역에 대한 mlock.
        let ret = unsafe { libc::mlock(ptr, len) };
        if ret != 0 {
            warn!(
                err = %std::io::Error::last_os_error(),
                len = len,
                "mlock failed — swapping 가능. ulimit memlock 을 unlimited 로 설정 권장"
            );
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn try_mlock_owned(&self) {}
}

// SAFETY: `ShmRegion` 자체는 raw pointer + Arc 만 보유. 접근 동기화는 상위 모듈의
// atomic 사용으로 보장된다. View 의 parent 는 Arc 이며 Arc<ShmRegion> 이 Send+Sync
// 이려면 ShmRegion 이 Send+Sync 여야 하므로 닫힌 귀납으로 성립.
unsafe impl Send for ShmRegion {}
unsafe impl Sync for ShmRegion {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn create_truncates_new_file_to_size() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("t1");
        let r = ShmRegion::create_or_attach(&p, 4096, false).unwrap();
        assert_eq!(r.len(), 4096);
        assert_eq!(p.metadata().unwrap().len(), 4096);
    }

    #[test]
    fn attach_existing_reuses_same_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("t2");
        let _r1 = ShmRegion::create_or_attach(&p, 8192, false).unwrap();
        let r2 = ShmRegion::attach_existing(&p).unwrap();
        assert_eq!(r2.len(), 8192);
    }

    #[test]
    fn create_rejects_mismatched_size() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("t3");
        let _r1 = ShmRegion::create_or_attach(&p, 4096, false).unwrap();
        let r2 = ShmRegion::create_or_attach(&p, 8192, false);
        assert!(r2.is_err());
    }

    #[test]
    fn rejects_zero_size() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("t4");
        let e = ShmRegion::create_or_attach(&p, 0, false);
        assert!(e.is_err());
    }

    #[test]
    fn ptrs_are_page_aligned() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("t5");
        let r = ShmRegion::create_or_attach(&p, 8192, false).unwrap();
        let addr = r.as_ptr() as usize;
        assert_eq!(addr % 4096, 0);
    }

    #[test]
    fn unlink_removes_file() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("t6");
        let r = ShmRegion::create_or_attach(&p, 4096, false).unwrap();
        r.unlink().unwrap();
        assert!(!p.exists());
    }

    // ───── sub_view tests ─────

    #[test]
    fn sub_view_pointer_arithmetic_matches_parent() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sv1");
        let parent = Arc::new(ShmRegion::create_or_attach(&p, 8192, false).unwrap());
        let view = ShmRegion::sub_view(parent.clone(), 1024, 2048).unwrap();
        assert_eq!(view.len(), 2048);
        assert!(view.is_view());
        let base = parent.as_ptr() as usize;
        let vbase = view.as_ptr() as usize;
        assert_eq!(vbase - base, 1024);
    }

    #[test]
    fn sub_view_writes_propagate_to_parent() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sv2");
        let parent = Arc::new(ShmRegion::create_or_attach(&p, 4096, false).unwrap());
        let mut view = ShmRegion::sub_view(parent.clone(), 128, 64).unwrap();
        // SAFETY: view 와 parent 는 같은 매핑. 테스트에서만 직접 포인터 write.
        unsafe {
            let dst = view.as_mut_ptr();
            *dst = 0xAB;
            *dst.add(63) = 0xCD;
        }
        unsafe {
            let base = parent.as_ptr();
            assert_eq!(*base.add(128), 0xAB);
            assert_eq!(*base.add(128 + 63), 0xCD);
            assert_eq!(*base.add(128 + 64), 0x00); // 범위 밖은 0.
        }
    }

    #[test]
    fn sub_view_rejects_out_of_range() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sv3");
        let parent = Arc::new(ShmRegion::create_or_attach(&p, 4096, false).unwrap());
        assert!(ShmRegion::sub_view(parent.clone(), 4000, 200).is_err());
        assert!(ShmRegion::sub_view(parent.clone(), 0, 0).is_err());
        assert!(ShmRegion::sub_view(parent, usize::MAX, 1).is_err());
    }

    #[test]
    fn sub_view_keeps_parent_alive() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sv4");
        let view = {
            let parent = Arc::new(ShmRegion::create_or_attach(&p, 4096, false).unwrap());
            ShmRegion::sub_view(parent, 0, 512).unwrap()
            // parent Arc 는 여기서 drop — 그러나 view 가 들고 있어 매핑은 살아 있어야 한다.
        };
        // 포인터 유효성 검사: len 읽기 + 첫 바이트 load 가 segfault 없이 성공해야.
        assert_eq!(view.len(), 512);
        unsafe {
            let _ = std::ptr::read_volatile(view.as_ptr());
        }
    }

    #[test]
    fn check_fd_size_covers_ok_when_equal() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("fd1");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&p)
            .unwrap();
        f.set_len(4096).unwrap();
        assert!(check_fd_size_covers(&f, 4096, &p).is_ok());
        assert!(check_fd_size_covers(&f, 2048, &p).is_ok());
    }

    #[test]
    fn check_fd_size_covers_err_when_short() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("fd2");
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&p)
            .unwrap();
        f.set_len(1024).unwrap();
        assert!(check_fd_size_covers(&f, 2048, &p).is_err());
    }

    #[test]
    fn sub_view_unlink_unsupported() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("sv5");
        let parent = Arc::new(ShmRegion::create_or_attach(&p, 4096, false).unwrap());
        let view = ShmRegion::sub_view(parent, 0, 1024).unwrap();
        assert!(matches!(view.unlink(), Err(ShmError::Unsupported(_))));
    }
}
