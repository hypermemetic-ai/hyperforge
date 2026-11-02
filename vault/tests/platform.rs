//! The heap the owner's tools run on, and the line between the tools and the
//! library. `src/platform.rs` belongs to the binaries, so this test takes it the
//! same way they do.

#[path = "../src/platform.rs"]
mod platform;

use platform::Zeroing;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Sees each block as it arrives to be freed: the only moment a test may look.
#[derive(Default)]
struct Witness {
    freed: AtomicUsize,
    dirty_bytes: AtomicUsize,
    forwarded_reallocs: AtomicUsize,
}

// SAFETY: a thin layer over System; `dealloc` only reads the block it was given.
unsafe impl GlobalAlloc for Witness {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        let bytes = std::slice::from_raw_parts(block, layout.size());
        self.dirty_bytes.fetch_add(bytes.iter().filter(|b| **b != 0).count(), Ordering::SeqCst);
        self.freed.fetch_add(1, Ordering::SeqCst);
        System.dealloc(block, layout);
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        self.forwarded_reallocs.fetch_add(1, Ordering::SeqCst);
        System.realloc(block, layout, size)
    }
}

#[test]
fn a_freed_block_arrives_at_the_system_zeroed() {
    let heap = Zeroing(Witness::default());
    for size in [1, 16, 4095, 4096, 64 * 1024, 300_000] {
        let layout = Layout::from_size_align(size, 8).unwrap();
        // SAFETY: a live block of `size` bytes, freed with its own layout.
        unsafe {
            let block = heap.alloc(layout);
            block.write_bytes(0xAB, size);
            heap.dealloc(block, layout);
        }
    }
    assert_eq!(heap.0.freed.load(Ordering::SeqCst), 6);
    assert_eq!(heap.0.dirty_bytes.load(Ordering::SeqCst), 0);
}

/// `Vec` growth and shrinking: a `realloc` passed down would move the bytes and
/// leave the old block as it was.
#[test]
fn a_block_that_moves_leaves_nothing_where_it_was() {
    let heap = Zeroing(Witness::default());
    let small = Layout::from_size_align(1024, 8).unwrap();
    // SAFETY: each pointer is live for its stated size, and freed once.
    unsafe {
        let block = heap.alloc(small);
        block.write_bytes(0xCD, 1024);
        let grown = heap.realloc(block, small, 512 * 1024);
        assert_eq!(*grown.add(1023), 0xCD, "the bytes moved with it");
        let large = Layout::from_size_align(512 * 1024, 8).unwrap();
        let shrunk = heap.realloc(grown, large, 16);
        assert_eq!(*shrunk.add(15), 0xCD);
        // The block still in use is the caller's to clear; `Witness` then sees
        // only what this layer wiped.
        shrunk.write_bytes(0, 16);
        heap.dealloc(shrunk, Layout::from_size_align(16, 8).unwrap());
    }
    assert_eq!(heap.0.forwarded_reallocs.load(Ordering::SeqCst), 0);
    assert_eq!(heap.0.dirty_bytes.load(Ordering::SeqCst), 0);
    assert_eq!(heap.0.freed.load(Ordering::SeqCst), 3);
}

/// macOS gives up on a page after 65535 locks, in the voice of a real limit. A
/// tool that reuses one small block forever must not get there. Only a Mac can
/// fail this; it is here for the day this runs on one.
#[test]
fn reusing_one_block_never_exhausts_the_locks() {
    if platform::lock_heap().is_err() {
        return;
    }
    let heap = Zeroing(System);
    let layout = Layout::from_size_align(32, 8).unwrap();
    let before = platform::lock_failures();
    for _ in 0..100_000 {
        // SAFETY: a live block, freed with its own layout.
        unsafe { heap.dealloc(heap.alloc(layout), layout) };
    }
    assert_eq!(platform::lock_failures(), before);
}

/// A tool that holds a token or a value frees it through the wiping heap. Read
/// from the source: what a process has already freed cannot be looked for once
/// it has exited, and `vault-login` exits as soon as its tokens are written.
#[test]
fn every_tool_that_holds_a_token_runs_on_the_wiping_heap() {
    for tool in ["vault-approve", "vault-encrypt", "vault-login"] {
        let path = format!("{}/src/bin/{tool}.rs", env!("CARGO_MANIFEST_DIR"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("#[global_allocator]\nstatic HEAP: platform::Zeroing = platform::Zeroing(std::alloc::System);"), "{tool}");
        assert!(text.contains("platform::undumpable()?;"), "{tool}");
    }
}

/// Nothing here may reach the code a mobile build shares.
#[test]
fn the_library_sets_no_process_policy() {
    let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let forbidden = ["global_allocator", "GlobalAlloc", "mlock", "setrlimit", "RLIMIT_", "prctl", "IsTerminal", "is_terminal", "isatty", "no_core_dumps", "mod platform"];
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") && path.file_name().unwrap() != "platform.rs" {
            let text = std::fs::read_to_string(&path).unwrap();
            for word in forbidden {
                assert!(!text.contains(word), "{} says {word}", path.display());
            }
            checked += 1;
        }
    }
    // client, enclave, keys, lib, login, manifest, owner, p256tag, server, wire.
    assert_eq!(checked, 10);
}

/// The library is what iOS shares: no command line, and no assumption about
/// where a home directory is. `login.rs` is handed its path.
#[test]
fn the_library_takes_no_cli_types_and_reads_no_home() {
    let dir = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") && path.file_name().unwrap() != "platform.rs" {
            let text = std::fs::read_to_string(&path).unwrap();
            for word in ["clap::", "clap(", "arg(", "command(", "\"HOME\""] {
                assert!(!text.contains(word), "{} says {word}", path.display());
            }
            checked += 1;
        }
    }
    // Not an exact count: this is about every file, whichever ones there are.
    assert!(checked >= 8, "{checked}");
}
