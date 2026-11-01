# UU native gamepad experiment / 原生手柄实验

Status: experimental and default OFF; opt in with `UU_VIEWER_NATIVE_GAMEPAD=1`.
One reporting user tested input, phone-hotspot WAN, reconnect and rumble on one
Windows/Steam Deck pair; the upstream maintainer has not independently tested it.
This does not establish compatibility with every official UU version.

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
with input reported working by the reporting user on their Windows/Deck pair.
No guessed new protobuf feature flags are advertised. Existing desktop negotiation is retained.

## Implementation and limits

`src/viewer/native_gamepad.rs` reads a single SDL GameController inside the desktop viewer.
It maps SDL buttons to XInput and reverses Y axes (including -32768 saturation), converts triggers
0..32767 to 0..255, uses slot 0, sends changes and a 100 ms state refresh. Guide button is omitted.
More than one mapped controller at discovery is deliberately ambiguous and none is opened.

Focus loss, local control menu, view-only, unplug or normal shutdown: send neutral then disconnect.
On entry/re-entry, require released buttons/triggers/sticks before arming. Back+Start toggles pause.
Normal cleanup uses bounded send attempts. Host behavior after SIGKILL/network loss remains
unverified; no host-side input watchdog interval has been established.
Very short taps entirely between SDL samples can be missed. Multi-controller is not implemented.
Logged markers describe local activity only; CONNECT_QUEUED is not an ACK.

## Rumble / 震动回传

`native_rumble.rs` parses the verified flat JSON `{action:"gpd_vibration",index:0,left_motor:0..255,right_motor:0..255}`
on the existing peer's CONTROL channel. It rejects malformed/duplicate fields, wrong types/ranges,
nonzero slot and messages over 512 bytes. The exact shape is backed by static inspection; receipt
on the CONTROL hook must still be verified on other Windows/Deck configurations; the reporting user has reported successful rumble on their pair.
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

Markers (no payload values): RUMBLE_SUPPORTED/UNAVAILABLE describe SDL capabilities;
RUMBLE_RECEIVED means a valid active-session message arrived; RUMBLE_APPLIED means SDL returned
success for a nonzero effect; RUMBLE_OUTPUT_FAILED means the output API failed. All have DECKPAD_ prefix.

## Validation / 验证

Use the CI checks on the current PR head as the automated validation record.
The tests cover protocol fields and ranges, activation/release gating, bounded
feedback delivery, finite rumble effects and shared CONTROL delivery alongside
file operations. Automated tests do not exercise physical controller feedback
or establish official-host compatibility.

自动验证以当前 PR 提交的 CI 结果为准；实机结果来自报告用户，不能视为上游维护者
已验收，也不涵盖全部主机版本、休眠、强制退出和断网恢复场景。

## Attribution / AI assistance

The native gamepad and rumble implementation, regression tests and this evidence document were
prepared with OpenAI Codex assistance. The user supplied requirements and reported the hardware
acceptance results. Existing project design, code and attribution belong to the upstream contributors.
