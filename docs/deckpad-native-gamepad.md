# UU native gamepad experiment / 原生手柄实验

Status: **v0.5 user acceptance passed** for input, phone-hotspot WAN, normal exit/display recovery
(with some delay) and reconnect. **v0.5.1 rumble was reported working by the user**.
No claim of compatibility with every official UU version. Default OFF; explicitly opt in with
`UU_VIEWER_NATIVE_GAMEPAD=1`. Legacy DeckPad tunnel experiments are not included in this fork.

## Evidence (2026-10-31)

Official Windows installer 4.45.0.4318, obtained from NetEase's release endpoint
`https://api.nrd.nie.163.com/api/v1/release/dl/1?channel=gwqd`.
Only inspected locally; no NetEase binaries/assets are included in this project or deliverable.

- `GameViewerServer.exe` SHA256 `af3a15c3887e06bfa9506971e01cd8ec58f41efe8ac9eb5831e31dae1fc1cef8`
- `GameViewer.exe` SHA256 `ef7112e173cdc6d74735c50292cf9750082ce351cdea60dc068fc2b60f3aa92e`
- All addresses below are RVAs, not absolute process addresses. Image base is 0x140000000.
- Lead: https://github.com/djkcyl/uu-enhance/blob/main/src/hooks.cpp identifies older
  GamepadManager Connect/Disconnect/Update log strings. Its server.cpp identifies corresponding
  GamepadManagerServer handlers. No hook code or DLL injection is used here.

Receiver static inspection:

| RVA | Observation |
| --- | --- |
| 0x5dea50 | Raw JSON handler skips heartbeat messages, reads `action`, routes prefix `gpd_` to dispatcher through thunk 0x657d0. |
| 0x5dde60 | Dispatcher recognizes `gpd_update`, `gpd_connect`, `gpd_disconnect`. Connect/disconnect read `index` as a byte. |
| 0x5ddb30 | State decoder requires `index`, `buttons`, `left_trigger`, `right_trigger`, `left_thumb_x`, `left_thumb_y`, `right_thumb_x`, `right_thumb_y`. |
| 0x5dfba0 | ViGEm output: low 16 bits of buttons, two 8-bit triggers and four 16-bit stick values copied into an XUSB report; target selected by index. Missing target attempts auto-connect; missing driver drops updates. |
| 0x5df5d0 | Vibration JSON builder: `action: gpd_vibration`, `index`, `left_motor`, `right_motor`; all three numeric values read as bytes. |
| 0x5dfe80 | ViGEm feedback callback constructs index/left/right byte record, calls vibration serializer, then 0x5df580 (via thunk 0x1c1e3) to forward JSON through its session interface. |

Official viewer corroboration:

- 0x10369d0 builds JSON object with `action: gpd_connect`, numeric `index`.
- 0x1036d20 builds disconnect object.
- 0x1037070 builds update object using exactly the receiver fields above, unsigned numeric
  buttons/index/triggers and signed thumb values.
- Viewer 0xc1c630 dispatches raw JSON action `gpd_vibration` alongside cursor notifications.
  Viewer 0x10331a0 parses index/left_motor/right_motor into three bytes.

This supports sending JSON through the existing binary CONTROL input path (`Peer::send_input`),
and v0.5 input acceptance has been reported by the user on their Windows/Deck pair.
No guessed new protobuf feature flags are advertised. Existing desktop negotiation is retained.

## Implementation and limits

`src/viewer/native_gamepad.rs` reads a single SDL GameController inside the desktop viewer.
It maps SDL buttons to XInput and reverses Y axes (including -32768 saturation), converts triggers
0..32767 to 0..255, uses slot 0, sends changes and a 100 ms state refresh. Guide button is omitted.
More than one mapped controller at discovery is deliberately ambiguous and none is opened.

Focus loss, local control menu, view-only, unplug or normal shutdown: send neutral then disconnect.
On entry/re-entry, require released buttons/triggers/sticks before arming. Back+Start toggles pause.
Normal cleanup uses bounded send attempts. Host behavior after SIGKILL/network loss remains
unverified; no promise that the native protocol has our previous 300 ms receiver watchdog.
Very short taps entirely between SDL samples can be missed. Multi-controller is not implemented.
Logged markers describe local activity only; CONNECT_QUEUED is not an ACK.

## v0.5.1 rumble / 震动回传

`native_rumble.rs` parses the verified flat JSON `{action:"gpd_vibration",index:0,left_motor:0..255,right_motor:0..255}`
on the existing peer's CONTROL channel. It rejects malformed/duplicate fields, wrong types/ranges,
nonzero slot and messages over 512 bytes. The exact shape is backed by static inspection; receipt
on the CONTROL hook must still be verified on other Windows/Deck configurations; the maintainer has reported successful rumble on their pair.
Only the current focused, armed SDL controller enables its peer-local inbox; a single latest-value slot
prevents a backlog from replaying after a pause. Feedback for inactive sessions is discarded.

Motor strength scales by 257 into SDL's 0..65535 range. SDL `set_rumble` receives left/low-frequency
and right/high-frequency values. Normal zero feedback stops immediately. Sustained rumble refreshes
every 250 ms with a finite 1000 ms hardware effect, because the host may send only state changes.
An active viewer keeps the last desired state until a new message or focus/connection/pause change;
there is no unsupported assumption that a quiet host must periodically repeat feedback.
Pending nonzero commands older than 500 ms are discarded; stop commands are always honored.
Pause, focus loss, view-only, unplug, detected peer disconnection and normal exit stop rumble and
discard the queue. A blocked/crashed viewer cannot keep renewing the 1000 ms SDL effect. Physical
network loss detection still depends on WebRTC; this is not a one-second network watchdog.
Output failure disables rumble until controller rediscovery/restart while preserving input.

SDL API reference: https://wiki.libsdl.org/SDL2/SDL_GameControllerRumble . A successful SDL call
does not prove the hardware moved. Steam Input/SteamOS rumble configuration and physical Deck
feedback remain dependent on each installation. This implements ordinary two-motor rumble, not adaptive
triggers or arbitrary touchpad haptic waveforms. `UU_VIEWER_NATIVE_RUMBLE=0` disables feedback.

The downloadable v0.5.1 test package included a one-time Windows XInputSetState test; its
left/right pulses were 35% and its combined pulse was 45%. The user reported working feedback.
The cloud test suite does not execute Windows XInput or physical controller hardware.

Markers (no payload values): RUMBLE_SUPPORTED/UNAVAILABLE describe SDL capabilities;
RUMBLE_RECEIVED means a valid active-session message arrived; RUMBLE_APPLIED means SDL returned
success for a nonzero effect; RUMBLE_OUTPUT_FAILED means the output API failed. All have DECKPAD_ prefix.

## Acceptance / 验收

1. Exit the old Steam entry; stop Windows Python receiver (foreground or background).
2. Start the native test viewer through the existing Steam entry; connect only one Windows PC.
3. Open Windows `joy.cpl`, controller Properties/Test. No `start_receiver.cmd` or reverse SSH tunnel.
4. Release all controls, then press A/B, move both sticks, and test both triggers independently.
5. Hold A, open Steam overlay / change focus / disconnect: confirm release; re-enter and test again.
6. Repeat via phone hotspot. Confirm no Python receiver process is needed on Windows.
7. Test reconnect and full exit; confirm controller cleanup and physical display restoration.

If no input appears, preserve the known-working v0.4.1 path and collect native markers plus official
Windows UU version/driver diagnostics. Do not silently fall back to the old receiver: that would
invalidate the test of native compatibility.

## Local validation for this experiment

- Clean upstream PR branch: 193 release-viewer tests passed, 1 pre-existing ignored,
  2 sandbox-restricted Unix IPC tests excluded. The two tests belonging solely to the former
  shared port-map experiment were removed with that experiment. CI keeps its full test command.
- v0.5.1: release viewer suite 195 passed, 1 pre-existing ignored, the same 2 Unix IPC tests
  excluded. Four new rumble tests cover strict parsing/scaling, slot isolation, bounded latest-stop
  delivery, focus/re-entry and finite-effect refresh. The existing virtual WebRTC/file-reattach test
  now also checks host-to-viewer nonzero and zero rumble feedback twice on the same CONTROL channel.
- v0.5.1: Python installer/marker tests 4 passed; shell syntax, release build and help smoke test.
  No physical controller, Windows XInput execution or official host rumble was exercised here.
- Release viewer tests: 191 passed, 1 pre-existing ignored, 2 local Unix IPC tests excluded
  (`server_client_roundtrip_and_uid_gate`, `oversized_frame_rejected`; these failed with socket
  permission restrictions during the earlier cloud build).
- This includes native JSON/range/state-gate tests and virtual WebRTC delivery of connect,
  A-down, neutral, disconnect over the existing CONTROL channel, interleaved with file RPCs.
- Final native module was also compiled directly as a Rust test crate with `viewer-sdl` enabled:
  3 passed after the hot-unplug/replacement ordering fix.
- Python installer/marker tests: 4 passed. No account, Windows host or physical controller used.
- Release binary build and `--help` smoke test are performed before packaging.

These results do NOT prove official Windows acceptance, actual Steam Deck input detection,
focus behavior of Steam overlay, rumble, WAN performance, or cleanup after forced exit.

## Attribution / AI assistance

The native gamepad and rumble implementation, regression tests and this evidence document were
prepared with OpenAI Codex assistance. The user supplied requirements and reported the hardware
acceptance results. Existing project design, code and attribution belong to the upstream contributors.
