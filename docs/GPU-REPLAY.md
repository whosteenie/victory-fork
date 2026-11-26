# Standalone GPU and page-mapping replays

The tests run on Windows from an x64 Visual Studio Developer PowerShell. They use
synthetic inputs and need no title installation, save, shader cache, or capture.
Run GPU timing on an idle hardware adapter. The parity test also runs on WARP
with the Windows D3D12 debug layer.

```powershell
.\diagnostics\Replay-Gpu.ps1 -Test Parity -Warp -Output "$env:TEMP\hash-parity.json"
.\diagnostics\Replay-Gpu.ps1 -Test Hash -Rounds 12 -Output "$env:TEMP\hash-ab.json"
.\diagnostics\Replay-Gpu.ps1 -Test Mapping -Rounds 100 -Output "$env:TEMP\mapping.json"
.\diagnostics\Replay-Gpu.ps1 -Test Indirect -Output "$env:TEMP\indirect.json"
.\diagnostics\Replay-Gpu.ps1 -List
.\diagnostics\Replay-Gpu.ps1 -Test Hash -Rounds 12 -CompareTo "$env:TEMP\hash-ab.json" -Output "$env:TEMP\hash-repeat.json"
```

The runner builds only the selected crate's test target. It records build and
run wall time, the commit, worktree status, executable SHA-256, warm-up and
measured rows, and summaries in JSON. GPU timing is in microseconds. P95 is
nearest rank. The two warm-up rounds appear in the raw results and are excluded
from the summaries.

## Add a workload

Register a named case in `diagnostics/Replay-Workloads.psd1`. A case points to
an exact Cargo library fixture and specifies its backend. Reuse an existing
test if it already covers the behavior. Provide its resource setup, deterministic
inputs, command recording, reset and assertions inside the fixture.

For a correctness-only case, Rust assertions are the verdict. If the test
can return early when a backend is unavailable, register a required `check`
record and emit that record only after the assertions finish. The counted
indirect fixture does this with its existing WARP path. It confirms the count
buffer limits execution, untouched input records and stride payload are preserved,
and both indirect executions charge the expected shared budget.

For timings, register the sample kind, variants, metrics, and configuration
record kinds. Each JSON line has a `kind` field. Samples also carry `mode`,
`round`, and `warmup`. Keep the setup parameters in a separate configuration
record so the runner can reject mismatched comparisons. New workloads reuse
the same build, execution, failure handling and reporting. GPU fixtures can
use `host::replay_context::CommandReplay` for a private direct list, allocator,
queue, timestamp frequency and fence wait. They own their resource bindings,
barriers and validity checks.

The reference comparison reports median and P95 deltas when the workload,
backend, round count, configuration and available hardware metadata agree.
Interactive capture and fault instrumentation are disabled for runner
measurements. Use repeated
measurements to set a meaningful regression threshold. The runner records
test failures as JSON and exits unsuccessfully. It rejects a timing comparison
with unknown GPU driver metadata. WARP parity, WARP indirect and Windows mapping
use independently generated inputs and can run in a Windows CI job alongside
the existing Cargo tests. The timed hardware workload is opt-in. No game path,
asset, save or driver-specific timing threshold is embedded in CI.

## Card hash replay

The branch starts at [fxceless ac25549](https://github.com/fxceless/victory-fork/commit/ac2554989dc5499c71947799de7203b82986a66c).
The test and renderer share the production FXC pipeline constructor, embedded
root signature, HLSL and dispatch sizing. The control runs the scalar card hash.
The candidate runs the grouped implementation from [PR 1014](https://github.com/fxceless/victory-fork/pull/1014).
A third case tests the existing adjacent-range joiner with the grouped hash.

The small parity case compares every stored hash against a CPU calculation for
both kernels, including skew and a partial final piece. It checks that
compare-only retains the old values, advancing comparison stores the new hash,
and another comparison clears the changed flag. The grouped kernel uses a
different hash representation. Both kernels must report the same exact
changed-piece outputs. WARP parity also checks for D3D12 debug errors.

The timed case uses one 174 MiB device-local input. It uploads 64 MiB in 256
ranges across 16 groups with one split for an edited record, rehashes 1,024 pieces, then compares 16,704 piece visits
over 96 advancing dispatches. This is a synthetic approximation of the observed
yard cadence of about 250 rehashes over 950 to 1,050 pieces and about 98 compares
over 16,800 pieces. Both variants keep the upload bytes and the comparison work
fixed. The join case combines rehashes only within each upload group.

Each sample restores its input and reseeds its hash representation. It poisons
the output flags before dispatch. A separate GPU copy edits one record between
the rehash and compare stages. The compare must find that edit and leave the
separate pre-rehash upload unflagged. Explicit transitions order input copies and shader
reads. UAV barriers order hash updates. A fence completes before reading the
timestamp and output mappings or reusing the allocator. Each round checks all
16,704 flags. The first compare must find the edit and later sweeps must see the
advanced hash.

The replay records CPU preparation, recording, submission and fence wait
separately. GPU timestamps bracket upload plus rehash and compare, excluding
the intervening synthetic write. Preparation includes unscored reseeding.
The replay uses one queue, one synthetic allocation and a fixed pattern. Live
lists span several allocations and interact with guest rendering, waits,
residency and multiple submissions. The replay measures the selected sequence
and its direction of change. Whole-frame performance and rendered output
require the game.

## Page mapping

The mapping replay uses XMem physical-page allocation, `map_alias`, writes
through mapped aliases, and physical-page release. It alternates two banks of
synthetic 64 KiB pages. Each round maps a consecutive run, replaces an
overlapping destination range, aliases a repeated source page, unmaps and refills
a hole, exercises a temporary page lifetime, and releases the views. It checks
every byte of each mapped page and uses a Windows memory query for placeholders.

The mapping operation samples exclude the page scans. The separate
allocation/map/free sample includes its lifetime checks. Kernel time is the
sum of current-thread kernel clock deltas over the measured `map_alias` calls.
Its granularity is coarse for short runs. The fixture starts at the mapper
used by `CopyPageMappingsBatchX`. Queue batch decoding, concurrent readers,
real streaming bursts, and frame impact still need integration testing.

## Pick the smallest test that covers the change

| Change | First check | When to run the game |
| --- | --- | --- |
| Pure shader translation | Shader unit and hardware parity fixture | When real descriptors, producer state, or rendering output are involved |
| Card hash or comparison scheduling | Parity then Hash A/B | Before claiming frame-time savings or changes in guest visibility |
| XMem mapping or batching | Mapping replay and existing XMem tests | To measure streaming hitches or verify interaction with live readers |
| Resource ordering or fence changes | Dependency fixture and ordered replay | If the behavior crosses queues, submissions, or guest CPU access |
| Pipeline selection or renderer architecture | Relevant GPU fixtures | Always for visual and gameplay validation |

Keep the existing cloned cafe checkpoint, cutscene skips and prison-yard scene
anchor for integration. Use matched scene windows and save the raw traces and
screenshots locally. The full replay can score mean, median, P95 and P99 and
capture shader activity, actual GPU commands, CPU preparation, synchronization,
memory budget, GPU faults and D3D12 errors. Record submitted and dropped work,
mesh and texture activity alongside the timing.

## External tools

- PIX can capture and replay D3D12 API calls and collect replay timings. GPU
  capture generally needs its matching capture DLL before device creation.
  Captures can be sensitive to devices and drivers. Obtain the official Windows
  installer and complete its setup and license prompts before enabling it.
  [GPU capture](https://devblogs.microsoft.com/pix/gpu-captures/) / [Downloads](https://devblogs.microsoft.com/pix/download/)
- Nsight Graphics 2026.2 provides a capture and standalone replay CLI. The
  installed CLI was verified. The compute fixture has no presentation boundary,
  so it uses its own timestamps. A representative VIctory frame capture remains
  unverified. [Capture and replay CLI](https://docs.nvidia.com/nsight-graphics/UserGuide/graphics-capture-cli.html)
- Nsight Systems 2022.4.2 generated a report containing D3D12 API records from
  the synthetic compute fixture. Use it for timelines, CPU submission and
  synchronization. Its report does not reconstruct executable D3D12 state.
  [Systems user guide](https://docs.nvidia.com/nsight-systems/UserGuide/index.html)
- Portable RenderDoc 1.47 captured the synthetic production parity sequence
  through VIctory's existing wrapper. XML export showed the buffer copies,
  root bindings, dispatches and queue signals. The native parity assertions
  passed during capture, and the CLI ran a two-loop replay. A scripted
  readback from fresh replay sessions remains to be verified. VIctory's
  upstream notes document a failure with 1.45 during a title launch.
  [Release](https://github.com/baldurk/renderdoc/releases/tag/v1.47)
- Native D3D12 timestamps and debug validation cover the replay. VIctory's
  existing frame shots, DRED reports and pipeline cache complement the
  standalone tests during game integration.

### Reproduce the synthetic capture

Run from an x64 Developer PowerShell. The ignored fixture uses an explicit
capture boundary because it runs compute commands without a swap chain. Set
the DLL and output prefix for just this process environment.

```powershell
$env:VICTORY_RENDERDOC = 'C:\tools\RenderDoc\renderdoc.dll'
$env:VICTORY_RENDERDOC_OUT = "$env:TEMP\victory-hash-parity"
cargo test --locked --profile diag -p victory-gpu --features diagnostics --lib host::card_replay::capture_card_hash_parity -- --exact --ignored --nocapture
Remove-Item Env:VICTORY_RENDERDOC,Env:VICTORY_RENDERDOC_OUT
```

Use `renderdoccmd convert --filename <capture.rdc> --output capture.xml --convert-format xml`
to inspect the recorded sequence, or `renderdoccmd replay --loops 2 <capture.rdc>`
to smoke-test execution. The CPU oracle is checked against live execution
during capture. The CLI smoke test does not compare replayed output bytes.

## Capture coverage roadmap

The existing `VICTORY_SHOT` output gives event order, timestamps and optional
resource images. It helps select a workload and estimate its shape. A replay
also needs the exact contents and versions of every input at the consumer
boundary. An image of a resource and its name cannot supply those dependencies.

Start with one direct-queue buffer sequence. Capture the native dispatch shape,
pipeline and root bindings, copy spans, required barriers, buffer descriptors,
immutable input bytes and expected readbacks. Take producer-dependent snapshots
at a fence boundary with known visibility. Use the same fixture runner to
compare repeat output and reject a capture with missing dependencies. Keep
actual captures local. Use independently generated equivalents for public CI
when they cover the same behavior.

Extend the RenderDoc proof to independently verified readbacks. Use PIX or
Nsight Graphics to test equivalent host-D3D12 capture where the RenderDoc
wrapper cannot capture the workload. An acceptance test should replay
the sequence twice and compare the resources read back from the original run and
both replays. Track resources by stable IDs and generation, with a local
manifest identifying the pin, adapter and driver. Measure host bindings and
submission order in VIctory itself when a tool cannot observe the guest-side
memory producer.

Current coverage includes buffer upload, rehash, producer update, compare and
CPU readback in one synthetic command sequence. Automatic capture and replay
of a real title workload is still unverified. Multi-queue dependencies,
external shared memory, sparse remaps, descriptor-heap mutations, live
resource lifetimes, and present ordering need dedicated contracts and tests.
Shader hot reload remains deferred until compatible replacement, rollback
and fence-based lifetimes can be checked in a standalone fixture.
