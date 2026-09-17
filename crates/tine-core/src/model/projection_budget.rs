//! The SQLite page-cache budget for the Direct Files projection writer.
//!
//! GH #543: a whole-graph build inserts every row of every UUID-keyed index
//! onto a random B-tree leaf, so its working set is a fixed multiple of the
//! text it projects. Below that multiple SQLite spills each dirty page to the
//! WAL and reads it back — 8.8M cache misses and 27 GB of reads for 49 MB of
//! Markdown at the ~2 MiB default on the reporter-scale fixture, while 512 MiB
//! finished the same build with 8,896 misses. The ceiling is allocated on
//! demand, so a generous budget costs a small graph nothing; the machine's
//! RAM caps it so an old laptop is not asked for more than it has.
//!
//! The multiple is a property of the schema, not of the graph: it was
//! measured on the 60-blocks-per-page GH #543 fixture with secondary indexes
//! built once after the rows (`gh543_build_probe` in tine-storage), and it is
//! re-measured, not re-derived, whenever the projection schema changes.

/// Bytes of page cache per byte of projected text during a bulk build.
///
/// Unit cost (2026-09-17, tine-storage `gh543_build_probe`, release, Linux,
/// deferred indexes; the projection stores ~32× its text, and the random-
/// access working set is about a third of that): cache misses per build at
/// 1,000 pages / 5.1 MB text — 2 MiB: 445k, 8: 180k, 16: 62k, 32: 14k,
/// 64: 13k, 512: 1; at 10,000 pages / 51 MB text — 64 MiB: 2.24M,
/// 128: 970k, 256: 173k, 512: 139k (build 42.5s → 34.3s). The knee sits at
/// 5–6 bytes of cache per text byte at both scales (32 MiB at 1k, 256 MiB at
/// 10k); below it the miss count roughly doubles per halving of the budget,
/// above it the curve is flat. 12 is the knee with 2× headroom, and it is a
/// ceiling: the cache fills only as far as the build's working set reaches.
pub(crate) const BUILD_CACHE_BYTES_PER_TEXT_BYTE: u64 = 12;

/// The smallest build budget worth setting: below this the reporter-scale
/// curve is already in the spill regime for any graph a user would notice.
pub(crate) const BUILD_CACHE_FLOOR_BYTES: u64 = 32 * MIB;

/// The largest build budget a single writer takes regardless of graph size.
pub(crate) const BUILD_CACHE_CEILING_BYTES: u64 = 768 * MIB;

/// The share of physical memory one projection build may claim.
pub(crate) const BUILD_CACHE_MEMORY_SHARE: u64 = 4;

/// The ceiling the writer keeps between builds, when it applies single-page
/// edits whose working set is one page's rows plus the indexes they touch.
pub(crate) const RESTING_CACHE_FLOOR_BYTES: u64 = 8 * MIB;
pub(crate) const RESTING_CACHE_CEILING_BYTES: u64 = 64 * MIB;

const MIB: u64 = 1024 * 1024;

/// The page-cache ceiling for a bulk build that projects `text_bytes` of
/// page and block text, on a machine with `physical_memory` bytes of RAM
/// (`None` when the platform query failed; the size-derived budget then
/// stands alone).
pub(crate) fn build_page_cache_budget(text_bytes: u64, physical_memory: Option<u64>) -> u64 {
    let wanted = text_bytes
        .saturating_mul(BUILD_CACHE_BYTES_PER_TEXT_BYTE)
        .clamp(BUILD_CACHE_FLOOR_BYTES, BUILD_CACHE_CEILING_BYTES);
    match physical_memory {
        Some(memory) => {
            wanted.min((memory / BUILD_CACHE_MEMORY_SHARE).max(BUILD_CACHE_FLOOR_BYTES))
        }
        None => wanted,
    }
}

/// The page-cache ceiling the writer returns to after a build.
pub(crate) fn resting_page_cache_budget(text_bytes: u64) -> u64 {
    text_bytes.clamp(RESTING_CACHE_FLOOR_BYTES, RESTING_CACHE_CEILING_BYTES)
}

/// Total physical memory in bytes, or `None` where the platform query fails.
/// Every shipped target has its own arm below; the fallback is reserved for
/// a platform Tine does not ship on, and `physical_memory_names_every_shipped_target`
/// keeps it that way.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn physical_memory_bytes() -> Option<u64> {
    // SAFETY: sysconf takes no pointers and has no preconditions.
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages <= 0 || page_size <= 0 {
        return None;
    }
    u64::try_from(pages)
        .ok()?
        .checked_mul(u64::try_from(page_size).ok()?)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
pub(crate) fn physical_memory_bytes() -> Option<u64> {
    let mut memory: u64 = 0;
    let mut length = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` is a u64 on every Apple platform; the out-buffer
    // and its length describe exactly that u64, and the name is NUL-terminated.
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            std::ptr::from_mut(&mut memory).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && length == std::mem::size_of::<u64>()).then_some(memory)
}

#[cfg(target_os = "windows")]
pub(crate) fn physical_memory_bytes() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX is plain data; dwLength must name its size
    // before the call, and the call writes only into it.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    (ok != 0).then_some(status.ullTotalPhys)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "windows",
)))]
pub(crate) fn physical_memory_bytes() -> Option<u64> {
    // Genuinely not a Tine platform (the five above are every shipped target).
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_graph_gets_the_floor_and_large_graph_the_ceiling() {
        assert_eq!(build_page_cache_budget(0, None), BUILD_CACHE_FLOOR_BYTES);
        // Martin-scale (4.5 MB of text): the multiple, not the floor.
        assert_eq!(
            build_page_cache_budget(4_500_000, None),
            4_500_000 * BUILD_CACHE_BYTES_PER_TEXT_BYTE
        );
        assert_eq!(
            build_page_cache_budget(10 * 1024 * MIB, None),
            BUILD_CACHE_CEILING_BYTES
        );
    }

    #[test]
    fn physical_memory_caps_the_budget_but_never_below_the_floor() {
        let reporter_scale = 49 * MIB;
        assert_eq!(
            build_page_cache_budget(reporter_scale, Some(16 * 1024 * MIB)),
            reporter_scale * BUILD_CACHE_BYTES_PER_TEXT_BYTE
        );
        assert_eq!(
            build_page_cache_budget(reporter_scale, Some(1024 * MIB)),
            256 * MIB
        );
        assert_eq!(
            build_page_cache_budget(reporter_scale, Some(64 * MIB)),
            BUILD_CACHE_FLOOR_BYTES
        );
    }

    #[test]
    fn resting_budget_tracks_text_within_its_band() {
        assert_eq!(resting_page_cache_budget(0), RESTING_CACHE_FLOOR_BYTES);
        assert_eq!(resting_page_cache_budget(20 * MIB), 20 * MIB);
        assert_eq!(
            resting_page_cache_budget(1024 * MIB),
            RESTING_CACHE_CEILING_BYTES
        );
    }

    #[test]
    fn this_machine_reports_its_memory() {
        let memory = physical_memory_bytes().expect("a shipped platform answers");
        assert!(memory >= 256 * MIB, "implausible physical memory {memory}");
    }

    /// A `cfg` family that omits a shipped target selects the `None` fallback
    /// there and silently drops the RAM cap on that platform only (AGENTS.md
    /// §2). The arms are pinned by name so the omission fails here instead.
    #[test]
    fn physical_memory_names_every_shipped_target() {
        let source = include_str!("projection_budget.rs");
        for target in ["linux", "android", "macos", "ios", "windows"] {
            let needle = format!("target_os = \"{target}\"");
            let arms = source.matches(&needle).count();
            assert!(
                arms >= 2,
                "physical_memory_bytes has no arm for {target} ({arms} mention(s)); \
                 every shipped target needs one, plus its line in the fallback's exclusion list"
            );
        }
    }
}
