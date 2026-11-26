//! Replay a synthetic streaming pattern through the Windows mapper.

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};

use super::{tests::query, *};

// VirtualFree flag to coalesce split placeholders before releasing the reservation.
const MEM_COALESCE_PLACEHOLDERS: u32 = 0x0000_0001;

const POOL_PAGES: usize = 64;
const VIEW_PAGES: usize = 24;

fn allocate(count: usize) -> Vec<usize> {
    let mut pages = vec![0; count];
    let mut actual = count;
    assert_eq!(
        XMemAllocatePhysicalPages(0, &mut actual, pages.as_mut_ptr()),
        1
    );
    assert_eq!(actual, count);
    pages
}

fn reserve(pages: usize) -> usize {
    let at = XMemVirtualAlloc(
        std::ptr::null_mut(),
        pages * PAGE,
        MEM_RESERVE | PAGES_64K,
        0,
        PAGE_READWRITE,
    );
    assert!(!at.is_null());
    at as usize
}

fn release(at: usize, pages: usize) {
    // SAFETY: this replay owns the entire unmapped placeholder reservation.
    unsafe {
        if pages > 1 {
            // A reservation that was never split already needs no coalescing.
            let _ = VirtualFree(
                at as _,
                pages * PAGE,
                MEM_RELEASE | MEM_COALESCE_PLACEHOLDERS,
            );
        }
        assert_ne!(
            VirtualFree(at as _, 0, MEM_RELEASE),
            0,
            "release placeholder reservation"
        );
    }
}

fn kernel_ticks() -> Option<u64> {
    let mut t = [FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }; 4];
    // SAFETY: four independent outputs for the current thread.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut t[0],
            &mut t[1],
            &mut t[2],
            &mut t[3],
        )
    };
    (ok != 0).then(|| u64::from(t[2].dwHighDateTime) << 32 | u64::from(t[2].dwLowDateTime))
}

fn step(round: usize, name: &str, pages: usize, elapsed: Duration) {
    println!(
        r#"{{"kind":"mapping_sample","mode":"{name}","round":{round},"warmup":{},"pages":{pages},"wall_us":{:.1}}}"#,
        round < 2,
        elapsed.as_secs_f64() * 1_000_000.0,
    );
}

fn verify(address: usize, expected: &[Option<u8>]) {
    for (i, byte) in expected.iter().enumerate() {
        let at = address + i * PAGE;
        let state = query(at as _).host.State;
        if let Some(value) = byte {
            assert_eq!(state, MEM_COMMIT, "view page {i} must be committed");
            // SAFETY: query established that this whole fixture page is
            // still mapped. This test owns its views and has no other threads.
            let data = unsafe { std::slice::from_raw_parts(at as *const u8, PAGE) };
            assert!(
                data.iter().all(|b| *b == *value),
                "wrong bytes in mapped view page {i}"
            );
        } else {
            assert_eq!(
                state, MEM_RESERVE,
                "view page {i} must be an unmapped placeholder"
            );
        }
    }
}

#[test]
#[ignore = "standalone Windows page-mapping benchmark; no GPU required"]
fn replay_page_mappings() {
    let rounds = std::env::var("VICTORY_REPLAY_ROUNDS")
        .ok()
        .map_or(10, |value| value.parse::<usize>().expect("integer rounds"));
    assert!(
        (2..=100).contains(&rounds),
        "choose 2 to 100 measured rounds"
    );
    let start = Instant::now();
    let pages = allocate(POOL_PAGES);
    let pool = reserve(POOL_PAGES);
    let view = reserve(VIEW_PAGES);
    assert_eq!(
        XMemMapPhysicalPages(pool as _, POOL_PAGES, pages.as_ptr()) as usize,
        pool
    );
    let mut expected = [None; VIEW_PAGES];
    let page_value = |index: usize| (index as u8).wrapping_mul(17).wrapping_add(3);
    let from_pool = |index: usize| pool + index * PAGE;
    let slot = |index: usize| view + index * PAGE;
    let map = |at: usize, source: usize, count: usize| {
        assert!(map_alias(at as u64, source as u64, (count * PAGE) as u64));
    };
    for (i, page) in pages.iter().enumerate() {
        assert!(write_physical(
            std::slice::from_ref(page),
            0,
            &vec![page_value(i); PAGE]
        ));
    }
    println!(
        r#"{{"kind":"mapping_init","version":1,"initialization_us":{:.1},"page_bytes":{PAGE},"pool_pages":{POOL_PAGES},"rounds":{rounds},"warmup_rounds":2}}"#,
        start.elapsed().as_secs_f64() * 1_000_000.0,
    );
    let mut mapping_kernel_ticks = kernel_ticks().map(|_| 0u64);
    for round in 0..rounds + 2 {
        let bank = (round % 2) * 32;
        let mut op = |name, count, action: &dyn Fn()| {
            let before = kernel_ticks();
            let elapsed = Instant::now();
            action();
            let duration = elapsed.elapsed();
            let kernel = kernel_ticks()
                .zip(before)
                .map(|(end, start)| end.saturating_sub(start));
            step(round, name, count, duration);
            if round >= 2 {
                mapping_kernel_ticks = mapping_kernel_ticks.zip(kernel).map(|(n, delta)| n + delta);
            }
        };
        op("map_run", 12, &|| map(slot(0), from_pool(bank), 12));
        for (i, value) in expected[..12].iter_mut().enumerate() {
            *value = Some(page_value(bank + i));
        }
        verify(view, &expected);

        op("overlap_remap", 10, &|| {
            map(slot(6), from_pool(bank + 12), 10)
        });
        for (i, value) in expected[6..16].iter_mut().enumerate() {
            *value = Some(page_value(bank + 12 + i));
        }
        verify(view, &expected);

        op("alias_repeat", 4, &|| {
            for i in 16..20 {
                map(slot(i), from_pool(bank + 12), 1);
            }
        });
        expected[16..20].fill(Some(page_value(bank + 12)));
        verify(view, &expected);
        // SAFETY: slot 19 and the pool page are still mapped read/write.
        // Fill through a second alias and check all the other views plus the pool.
        let edited = page_value(bank + 12) ^ 0xff;
        unsafe {
            (slot(19) as *mut u8).write_bytes(edited, PAGE);
        }
        expected[6] = Some(edited);
        expected[16..20].fill(Some(edited));
        verify(view, &expected);
        verify(from_pool(bank + 12), &[Some(edited)]);

        op("unmap_middle", 4, &|| map(slot(4), 0, 4));
        expected[4..8].fill(None);
        verify(view, &expected);
        op("remap_hole", 4, &|| map(slot(4), from_pool(bank + 22), 4));
        for (i, value) in expected[4..8].iter_mut().enumerate() {
            *value = Some(page_value(bank + 22 + i));
        }
        verify(view, &expected);
        assert!(write_physical(
            &pages[bank + 12..bank + 13],
            0,
            &vec![page_value(bank + 12); PAGE]
        ));

        // Exercise lifecycle independently of the pool's persistent pages.
        let lifetime = Instant::now();
        let temporary = allocate(1);
        assert_eq!(
            XMemMapPhysicalPages(slot(22) as _, 1, temporary.as_ptr()) as usize,
            slot(22)
        );
        assert!(write_physical(&temporary, 0, &vec![0xa7; PAGE]));
        verify(slot(22), &[Some(0xa7)]);
        map(slot(22), 0, 1);
        assert_eq!(XMemFreePhysicalPages(1, temporary.as_ptr()), 1);
        verify(slot(22), &[None]);
        step(round, "allocate_map_free", 1, lifetime.elapsed());

        op("unmap_batch", 20, &|| map(slot(0), 0, 20));
        expected.fill(None);
        verify(view, &expected);
    }
    map(pool, 0, POOL_PAGES);
    assert_eq!(XMemFreePhysicalPages(POOL_PAGES, pages.as_ptr()), 1);
    release(view, VIEW_PAGES);
    release(pool, POOL_PAGES);
    let kernel = mapping_kernel_ticks.map_or("null".to_owned(), |ticks| {
        format!("{:.1}", ticks as f64 / 10.0)
    });
    println!(
        r#"{{"kind":"mapping_summary","measured_mapping_calls":{},"kernel_us":{kernel}}}"#,
        rounds * 9
    );
}
