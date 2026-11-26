//! Reusable fenced command recording for standalone native D3D12 fixtures.

use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::*;

struct Event(HANDLE);

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: this fixture owns the event.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A direct queue, allocator and list owned by one standalone test.
pub(crate) struct CommandReplay {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: Event,
    next: u64,
    pub(crate) frequency: f64,
}

impl CommandReplay {
    pub(crate) fn new(device: &ID3D12Device) -> Self {
        // SAFETY: every D3D12 object created here belongs to this fixture.
        unsafe {
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
            Self {
                queue,
                allocator,
                list,
                fence,
                event,
                next: 0,
                frequency,
            }
        }
    }

    pub(crate) fn begin(&self) {
        // SAFETY: submit waited for the preceding use of this allocator.
        unsafe {
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
        }
    }

    pub(crate) fn submit(&mut self) -> (f64, f64) {
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
}

pub(crate) fn micros(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1_000_000.0
}
