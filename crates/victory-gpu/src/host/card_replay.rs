//! Standalone replay of the compatibility layer's resident-buffer hash passes.

use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::*;
use crate::shadow::{CardSpan, joined_piece_runs};

const PIECE: u32 = 65536;
const UNCHANGED_FLAG: u32 = 0xdead_beef;
type Pipeline = (ID3D12RootSignature, ID3D12PipelineState);

struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: this fixture owns the event.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct Replay {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: Event,
    next: u64,
    query: ID3D12QueryHeap,
    ticks: ID3D12Resource,
    tick_data: *mut u8,
    frequency: f64,
    pipeline: [Pipeline; 2],
    input: ID3D12Resource,
    upload: ID3D12Resource,
    hashes: ID3D12Resource,
    flags: ID3D12Resource,
    flags_data: *mut u8,
    flag_words: usize,
}

impl Replay {
    fn new(bytes: u32, pieces: u32, flag_words: usize) -> Self {
        let host = host().expect("D3D12 software or hardware device");
        let device = host.device();
        let pipeline = [false, true]
            .map(|wide| make_card_compare(device, wide).expect("production hash pipeline"));
        let mut query = None;
        // SAFETY: every D3D12 object created here belongs to this fixture.
        let (queue, allocator, list, fence, event, frequency) = unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .unwrap();
            list.Close().unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let event = Event(CreateEventW(None, false, false, None).unwrap());
            let frequency = queue.GetTimestampFrequency().unwrap() as f64;
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 4,
                        NodeMask: 0,
                    },
                    &mut query,
                )
                .unwrap();
            (queue, allocator, list, fence, event, frequency)
        };
        let (upload, upload_data) = host.upload_buffer(u64::from(bytes) + 16).unwrap();
        let input = host.uav_buffer(u64::from(bytes)).unwrap();
        let hashes = host.uav_buffer(u64::from(pieces) * 16).unwrap();
        let (flags, flags_data) = host.shared_buffer(flag_words as u64 * 4).unwrap();
        let (ticks, tick_data) = host.shared_buffer(32).unwrap();
        // SAFETY: the mappings cover these words and have no in-flight users.
        unsafe {
            let upload_words =
                std::slice::from_raw_parts_mut(upload_data.cast::<u32>(), bytes as usize / 4 + 4);
            for (index, word) in upload_words.iter_mut().enumerate() {
                let index = index as u32;
                *word = index.wrapping_mul(0x9e37_79b9) ^ index.rotate_left(13);
            }
            upload_words[bytes as usize / 4..].copy_from_slice(&[
                0x7654_3210,
                0x0201_00ff,
                0xabcdefe1,
                0xf00d_cafe,
            ]);
            std::slice::from_raw_parts_mut(flags_data.cast::<u32>(), flag_words)
                .fill(UNCHANGED_FLAG);
        }
        let mut replay = Self {
            queue,
            allocator,
            list,
            fence,
            event,
            next: 0,
            query: query.unwrap(),
            ticks,
            tick_data,
            frequency,
            pipeline,
            input,
            upload,
            hashes,
            flags,
            flags_data,
            flag_words,
        };
        replay.begin();
        // SAFETY: the newly allocated input is COMMON and only this list uses it.
        unsafe {
            replay
                .list
                .CopyBufferRegion(&replay.input, 0, &replay.upload, 0, u64::from(bytes));
        }
        replay.submit();
        replay
    }

    fn begin(&self) {
        // SAFETY: submit waited for the preceding use of this allocator.
        unsafe {
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
        }
    }

    fn submit(&mut self) -> (f64, f64) {
        let start = Instant::now();
        self.next += 1;
        // SAFETY: the closed list is this allocator's only work, and resources
        // stay live until the fence signals. A timed-out test aborts the sample.
        unsafe {
            self.list.Close().unwrap();
            self.queue
                .ExecuteCommandLists(&[Some(self.list.cast().unwrap())]);
            self.queue.Signal(&self.fence, self.next).unwrap();
            let submit = micros(start);
            let waiting = Instant::now();
            self.fence
                .SetEventOnCompletion(self.next, self.event.0)
                .unwrap();
            assert_eq!(
                WaitForSingleObject(self.event.0, 30_000),
                WAIT_OBJECT_0,
                "GPU fence timeout"
            );
            assert_eq!(
                self.fence.GetCompletedValue(),
                self.next,
                "GPU fence/device failure"
            );
            (submit, micros(waiting))
        }
    }

    fn bind(&self, wide: bool) {
        let (root, pso) = &self.pipeline[usize::from(wide)];
        // SAFETY: this list is recording and the bindings match the production root signature.
        unsafe {
            self.list.SetComputeRootSignature(root);
            self.list.SetPipelineState(pso);
            self.list
                .SetComputeRootShaderResourceView(1, self.input.GetGPUVirtualAddress());
            self.list
                .SetComputeRootUnorderedAccessView(2, self.hashes.GetGPUVirtualAddress());
            self.list
                .SetComputeRootUnorderedAccessView(3, self.flags.GetGPUVirtualAddress());
        }
    }

    fn dispatch(&self, wide: bool, words: [u32; 6]) {
        // SAFETY: the production root signature accepts all six constants, and
        // the caller's span and output offset fit the fixture's buffers.
        unsafe {
            self.list
                .SetComputeRoot32BitConstants(0, 6, words.as_ptr().cast(), 0);
            self.list
                .Dispatch(card_compare_groups(wide, words[3]), 1, 1);
        }
    }

    fn copy_input(&self, from: u64, to: u64, len: u64) {
        // SAFETY: this list starts with COMMON input, promoted by the copy.
        // The following shader reads that same resource in its transitioned state.
        unsafe {
            self.list
                .CopyBufferRegion(&self.input, to, &self.upload, from, len);
            self.list.ResourceBarrier(&[transition(
                &self.input,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            )]);
        }
    }

    fn input_state(&self, before: D3D12_RESOURCE_STATES, after: D3D12_RESOURCE_STATES) {
        // SAFETY: updates the whole live input buffer during this recording.
        unsafe {
            self.list
                .ResourceBarrier(&[transition(&self.input, before, after)]);
        }
    }

    fn order_hashes(&self) {
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
            ..Default::default()
        };
        // SAFETY: this orders earlier UAV writes before the next hash readers/writers.
        unsafe {
            self.list.ResourceBarrier(&[barrier]);
        }
    }

    fn flags(&self) -> &[u32] {
        // SAFETY: submit completed and the mapped resource outlives this view.
        unsafe { std::slice::from_raw_parts(self.flags_data.cast::<u32>(), self.flag_words) }
    }

    fn hash_words(&mut self, pieces: u32) -> Vec<[u32; 4]> {
        let n = u64::from(pieces) * 16;
        let (readback, data) = host().unwrap().shared_buffer(n).unwrap();
        self.begin();
        // SAFETY: separate execution with both buffers in COMMON, and the
        // source and destination ranges contain exactly n bytes.
        unsafe {
            self.list.CopyBufferRegion(&readback, 0, &self.hashes, 0, n);
        }
        self.submit();
        // SAFETY: the readback fence has completed before the mapped read.
        unsafe { std::slice::from_raw_parts(data.cast::<[u32; 4]>(), pieces as usize).to_vec() }
    }
}

fn micros(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1_000_000.0
}

/// CPU reference over an independently generated input. The modes have
/// different hash representations. Exact words are compared within each mode.
fn reference(input: &[u32], piece: u32, skew: u32, size: u32, wide: bool) -> [u32; 4] {
    const SEED: [u32; 4] = [0x9e37_79b9, 0x85eb_ca6b, 0xc2b2_ae35, 0x27d4_eb2f];
    let first = piece.saturating_mul(PIECE).saturating_sub(skew) / 16;
    let end = ((piece + 1) * PIECE - skew).min(size).div_ceil(16);
    let mix = |hash: [u32; 4], word: [u32; 4]| {
        let primes = [0x0100_0193, 0x9e37_79b1, 0x85eb_ca77, 0xc2b2_ae3d];
        let product: [u32; 4] =
            std::array::from_fn(|i| (hash[i] ^ word[i]).wrapping_mul(primes[i]));
        std::array::from_fn(|i| product[(i + 1) % 4] ^ (product[i] >> 15))
    };
    let words = |index: u32| {
        input[(index * 4) as usize..(index * 4 + 4) as usize]
            .try_into()
            .unwrap()
    };
    if !wide {
        return (first..end).fold(SEED, |hash, index| mix(hash, words(index)));
    }
    (0..64).fold(
        [0x27d4_eb2f, 0x1656_67b1, 0xd3a2_646c, 0xfd70_46c5],
        |group, lane| {
            let partial = (first + lane..end)
                .step_by(64)
                .fold(SEED.map(|seed| seed ^ lane), |hash, index| {
                    mix(hash, words(index))
                });
            mix(group, partial)
        },
    )
}

fn parity() {
    const BYTES: u32 = 2 * PIECE + 16;
    const SIZE: u32 = 2 * PIECE + 4;
    const SKEW: u32 = 12;
    let mut rig = Replay::new(BYTES, 3, 5);
    let pristine: Vec<_> = (0..BYTES / 4)
        .map(|i| i.wrapping_mul(0x9e37_79b9) ^ i.rotate_left(13))
        .collect();
    let mut updated = pristine.clone();
    updated[..4].copy_from_slice(&[0x7654_3210, 0x0201_00ff, 0xabcdefe1, 0xf00d_cafe]);
    for wide in [false, true] {
        let expected = (0..3)
            .map(|i| reference(&pristine, i, SKEW, SIZE, wide))
            .collect::<Vec<_>>();
        let after = (0..3)
            .map(|i| reference(&updated, i, SKEW, SIZE, wide))
            .collect::<Vec<_>>();
        rig.begin();
        rig.copy_input(0, 0, u64::from(BYTES));
        rig.bind(wide);
        rig.dispatch(wide, [0, SKEW, SIZE, 3, 4, 0]);
        rig.submit();
        assert_eq!(rig.hash_words(3), expected);
        rig.begin();
        rig.copy_input(u64::from(BYTES), 0, 16);
        rig.bind(wide);
        rig.dispatch(wide, [0, SKEW, SIZE, 3, 4, 1]);
        rig.submit();
        assert_eq!(rig.flags(), &[UNCHANGED_FLAG, 1, 0, 0, UNCHANGED_FLAG]);
        assert_eq!(
            rig.hash_words(3),
            expected,
            "compare-only changed a stored hash"
        );
        rig.begin();
        rig.bind(wide);
        rig.dispatch(wide, [0, SKEW, SIZE, 3, 4, 2]);
        rig.submit();
        assert_eq!(rig.flags(), &[UNCHANGED_FLAG, 1, 0, 0, UNCHANGED_FLAG]);
        assert_eq!(rig.hash_words(3), after);
        rig.begin();
        rig.bind(wide);
        rig.dispatch(wide, [0, SKEW, SIZE, 3, 4, 1]);
        rig.submit();
        assert_eq!(rig.flags(), &[UNCHANGED_FLAG, 0, 0, 0, UNCHANGED_FLAG]);
    }
}

#[test]
#[ignore = "standalone D3D12 compare correctness, use hardware or diagnostics WARP"]
fn card_hash_parity() {
    parity();
}

#[test]
#[ignore = "standalone native GPU timing, run on an otherwise idle device"]
fn replay_card_hashes() {
    assert!(
        std::env::var_os("VICTORY_D3D12_WARP").is_none(),
        "timing needs a hardware adapter"
    );
    const BYTES: u32 = 174 * 1024 * 1024;
    const PIECES: u32 = BYTES / PIECE;
    const COMPARE_SPANS: u32 = 16;
    const COMPARE_SWEEPS: u32 = 6;
    const COMPARE_PIECES: u32 = PIECES * COMPARE_SWEEPS;
    // One recorded Series X yard window reported about 250 uploads/hashes,
    // 950-1050 rehashed pieces, and 98 compares over about 16,800 pieces.
    // This schedule approximates that shape with wholly synthetic bytes.
    let mut first = 0;
    let spans: Vec<_> = [1, 2, 5, 8]
        .into_iter()
        .cycle()
        .take(256)
        .map(|pieces| {
            let from = first * PIECE;
            let run = CardSpan {
                first,
                pieces,
                skew: 0,
                from,
                to: from + pieces * PIECE,
            };
            first += pieces;
            run
        })
        .collect();
    // The live run recorded 18-19 upload lists/frame. Keep 16 successive
    // groups with their own copy/read boundaries and join only within a group.
    let flights: Vec<_> = spans.chunks(16).map(|spans| spans.to_vec()).collect();
    let joined: Vec<_> = flights.iter().cloned().map(joined_piece_runs).collect();
    for (original, coalesced) in flights.iter().zip(&joined) {
        let piece_ids = |spans: &[CardSpan]| {
            spans
                .iter()
                .flat_map(|s| s.first..s.first + s.pieces)
                .collect::<Vec<_>>()
        };
        assert_eq!(piece_ids(original), piece_ids(coalesced));
    }
    let rounds = std::env::var("VICTORY_REPLAY_ROUNDS")
        .ok()
        .map_or(10, |v| v.parse::<usize>().expect("integer rounds"));
    assert!(
        (2..=100).contains(&rounds),
        "choose 2 to 100 measured rounds"
    );
    let initialize = Instant::now();
    let host = host().expect("D3D12 hardware device");
    // SAFETY: queries the adapter already chosen by production Host.
    let desc = unsafe { host.adapter.as_ref().expect("adapter").GetDesc1() }.unwrap();
    assert_eq!(
        desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32,
        0,
        "hardware required"
    );
    println!(
        "Replay adapter: {}",
        String::from_utf16_lossy(&desc.Description).trim_end_matches('\0')
    );
    parity();
    let mut rig = Replay::new(BYTES, PIECES, COMPARE_PIECES as usize);
    println!(
        r#"{{"kind":"init","version":1,"initialization_us":{:.1},"input_bytes":{BYTES},"rehash_pieces":{first},"compare_pieces":{COMPARE_PIECES},"compare_dispatches":{},"upload_groups":{},"warmup_rounds":2,"rounds":{rounds}}}"#,
        micros(initialize),
        COMPARE_SPANS * COMPARE_SWEEPS,
        flights.len()
    );
    let modes = [
        ("scalar", false, &flights),
        ("grouped", true, &flights),
        ("grouped_joined", true, &joined),
    ];
    for (mode, _, rehash) in modes {
        let dispatches: usize = rehash.iter().map(Vec::len).sum();
        println!(r#"{{"kind":"mode","mode":"{mode}","rehash_dispatches":{dispatches}}}"#);
    }
    for round in 0..rounds + 2 {
        for position in 0..modes.len() {
            let (mode, wide, rehash) = modes[(round + position) % modes.len()];
            // The stored representation belongs to this mode. Seed all pieces
            // before timing so neither another mode nor an earlier round can
            // supply its hash words.
            rig.begin();
            rig.copy_input(0, 0, 16);
            rig.bind(wide);
            rig.dispatch(wide, [0, 0, BYTES, PIECES, 0, 0]);
            rig.submit();

            let started = Instant::now();
            rig.begin();
            // SAFETY: all four queries are resolved after the two timed command ranges.
            unsafe {
                rig.list.EndQuery(&rig.query, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            }
            rig.bind(wide);
            for (index, (upload, hashes)) in flights.iter().zip(rehash).enumerate() {
                if index != 0 {
                    rig.input_state(
                        D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                    );
                }
                for span in upload {
                    // SAFETY: COPY_DEST, implicit on the first upload, with
                    // the same byte copies in all three variants.
                    unsafe {
                        rig.list.CopyBufferRegion(
                            &rig.input,
                            u64::from(span.from),
                            &rig.upload,
                            u64::from(span.from),
                            u64::from(span.to - span.from),
                        );
                    }
                }
                rig.input_state(
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                );
                for span in hashes {
                    rig.dispatch(wide, [span.first, span.skew, BYTES, span.pieces, 0, 0]);
                }
            }
            // Hash writes must be visible before the later compare reads them.
            rig.order_hashes();
            // SAFETY: this end query follows all the rehash groups.
            unsafe {
                rig.list.EndQuery(&rig.query, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            }
            rig.input_state(
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            rig.copy_input(u64::from(BYTES), 0, 16);
            unsafe {
                rig.list.EndQuery(&rig.query, D3D12_QUERY_TYPE_TIMESTAMP, 2);
            }
            let chunk = PIECES / COMPARE_SPANS;
            for sweep in 0..COMPARE_SWEEPS {
                for span in 0..COMPARE_SPANS {
                    let at = span * chunk;
                    rig.dispatch(wide, [at, 0, BYTES, chunk, (sweep * PIECES + at) * 4, 2]);
                }
                rig.order_hashes();
            }
            unsafe {
                rig.list.EndQuery(&rig.query, D3D12_QUERY_TYPE_TIMESTAMP, 3);
                rig.list.ResolveQueryData(
                    &rig.query,
                    D3D12_QUERY_TYPE_TIMESTAMP,
                    0,
                    4,
                    &rig.ticks,
                    0,
                );
            }
            let record = micros(started);
            let (submit, wait) = rig.submit();
            // SAFETY: fence completed before the query/result mappings are read.
            let [a, b, c, d] = unsafe { rig.tick_data.cast::<[u64; 4]>().read() };
            assert!(a < b && b <= c && c < d, "invalid GPU timestamps");
            assert_eq!(
                rig.flags()[0],
                1,
                "the GPU edit must be found on its first comparison"
            );
            assert!(
                rig.flags()[1..].iter().all(|&flag| flag == 0),
                "advancing hash or unchanged piece mismatch"
            );
            let ticks_us = |delta| delta as f64 * 1_000_000.0 / rig.frequency;
            println!(
                r#"{{"kind":"sample","mode":"{mode}","round":{round},"warmup":{},"record_us":{record:.1},"submit_us":{submit:.1},"wait_us":{wait:.1},"upload_hash_us":{:.1},"compare_us":{:.1},"checked_flags":{COMPARE_PIECES}}}"#,
                round < 2,
                ticks_us(b - a),
                ticks_us(d - c)
            );
        }
    }
}
