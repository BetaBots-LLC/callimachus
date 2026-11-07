//! Return memory the allocator freed but kept to the OS.
//!
//! Why: after the MCP server dropped its embedding model, macOS's malloc still held the
//! freed ONNX Runtime buffers as "MALLOC_LARGE (empty)" regions (about 94 MB of a 214 MB
//! footprint, measured with `vmmap --summary`), which count against the process until the
//! system comes under memory pressure. Asking for relief right after a big free (a model
//! unload, a page-cache shrink) makes the unload show up in Activity Monitor at once.

/// Ask the system allocator to give its freed, cached pages back. Best effort, a few ms at
/// most; a no-op where there is no such call.
pub fn release_freed_memory() {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: a null zone means "every zone" and a goal of 0 means "as much as you can";
        // the call only releases memory no allocation is using.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: glibc's malloc_trim only returns free heap memory to the OS.
        unsafe {
            malloc_trim(0);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn release_is_safe_to_call_repeatedly() {
        let big = vec![1u8; 64 << 20];
        drop(std::hint::black_box(big));
        super::release_freed_memory();
        super::release_freed_memory();
    }
}
