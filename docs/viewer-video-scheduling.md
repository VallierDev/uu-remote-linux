# SDL viewer video scheduling

## Problem and cause

The SDL viewer previously consumed at most one compressed access unit per 8 ms
tick: approximately 125 units/s. With one unit per picture, sustained 144 FPS
input can overflow its bounded queue even when decoding keeps up. Lost compressed
units can break reference-frame dependencies, causing repeated decoder resets
and keyframe recovery. Increasing queue capacity alone only delays the overflow.

## Implementation

Each tick feeds queued compressed units to the existing decoder in FIFO order,
up to 8 units. A 4 ms elapsed-work budget is checked between calls. The viewer
then presents only the newest completed picture, avoiding redundant presentation
of older decoded pictures. It also polls asynchronous decoder output on idle ticks.

The compressed queue holds 8 units instead of 3 to absorb short presentation
stalls; processing starts immediately without waiting for a full batch. Actual
compressed-unit loss still resets the decoder and uses the existing keyframe
gate and PLI requests. Decoder interfaces, counters and opt-in diagnostic dumps
remain available; the presentation counter counts pictures actually presented.
There are no new settings or protocol fields.

## Verification

The desktop regression tests simulate 8 seconds of 144 units/s input and 125
processing ticks/s. The old one-unit policy drops 150 units with capacity 3 and
145 with capacity 8. Bounded batching delivers all 1,152 units to a mock decoder
in order without compressed-unit loss or resets, presenting at most once per tick.
Other tests cover newest-picture selection, idle asynchronous output, recovery
after real queue overflow, and count/time budgets leaving work for the next tick.
Budget tests inject elapsed time internally rather than depending on CI timing.

Run the full viewer checks without test filters or skips:

```sh
cargo test --locked --features native-viewer,viewer-sdl --bin uuremote-viewer
cargo build --locked --features native-viewer,viewer-sdl --bin uuremote-viewer
```

The current PR commit's CI is the authoritative integration result. Hardware
feedback on an earlier combined build came from the reporting user, not the
maintainer; it is supplementary and does not validate this isolated change.
Real-device acceptance of this isolated patch has not been performed.

## Limits and scope

The 4 ms budget cannot interrupt an already-blocked decoder call and does not
bound the subsequent presentation call. Slow decoding or rendering can still
overflow the queue. This adds no hardware decoding and guarantees neither stable
144 FPS nor 144 distinct displayed pictures on a 90 Hz screen.

Startup timeout, stream-profile synchronization, gamepad/rumble and installation
changes are outside this patch. It is independent of #142's gamepad work.
Implementation and regression tests were prepared with Codex assistance.
