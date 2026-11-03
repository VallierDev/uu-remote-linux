// HarmonyOS screen -> encoder surface; event injection uses local system consent.
#include <multimedia/player_framework/native_avscreen_capture.h>
#include <multimedia/player_framework/native_avcodec_videoencoder.h>
#include <multimedia/player_framework/native_avformat.h>
#include <multimedia/player_framework/native_avbuffer.h>
#include <native_window/external_window.h>
#include <multimodalinput/oh_input_manager.h>
#include <multimodalinput/oh_key_code.h>
#include <hilog/log.h>
#include <algorithm>
#include <atomic>
#include <cstring>
#include <deque>
#include <mutex>
#include <set>
#include <string>
#include <vector>

namespace steamsir::ohos {
// Standard X11 keysyms emitted by the shared Windows VK mapping.
constexpr unsigned long kBackspace = 0xff08;
constexpr unsigned long kTab = 0xff09;
constexpr unsigned long kEnter = 0xff0d;
constexpr unsigned long kKeypadEnter = 0xff8d;
constexpr unsigned long kEscape = 0xff1b;
constexpr unsigned long kDelete = 0xffff;
constexpr unsigned long kHome = 0xff50;
constexpr unsigned long kLeft = 0xff51;
constexpr unsigned long kUp = 0xff52;
constexpr unsigned long kRight = 0xff53;
constexpr unsigned long kDown = 0xff54;
constexpr unsigned long kPageUp = 0xff55;
constexpr unsigned long kPageDown = 0xff56;
constexpr unsigned long kEnd = 0xff57;
constexpr unsigned long kInsert = 0xff63;
constexpr unsigned long kShiftLeft = 0xffe1;
constexpr unsigned long kShiftRight = 0xffe2;
constexpr unsigned long kControlLeft = 0xffe3;
constexpr unsigned long kControlRight = 0xffe4;
constexpr unsigned long kCapsLock = 0xffe5;
constexpr unsigned long kAltLeft = 0xffe9;
constexpr unsigned long kAltRight = 0xffea;
constexpr unsigned long kMetaLeft = 0xffeb;
constexpr unsigned long kMetaRight = 0xffec;
constexpr unsigned long kNumLock = 0xff7f;
constexpr unsigned long kScrollLock = 0xff14;
constexpr unsigned long kPause = 0xff13;
constexpr unsigned long kFunctionFirst = 0xffbe;
constexpr unsigned long kFunctionLast = 0xffc9;
constexpr unsigned long kKeypadDigitFirst = 0xffb0;
constexpr unsigned long kKeypadDigitLast = 0xffb9;
constexpr size_t kMaxFrames = 4;
constexpr size_t kMaxFrameBytes = 8 * 1024 * 1024;
std::mutex api_mutex;
std::mutex frame_mutex;
std::mutex ui_mutex;
std::mutex input_mutex;
OH_AVCodec* encoder = nullptr;
OH_AVScreenCapture* capture = nullptr;
OHNativeWindow* surface = nullptr;
std::deque<std::vector<uint8_t>> frames;
std::vector<uint8_t> codec_data;
bool needs_idr = true;
std::atomic<bool> accept_input{false};
std::atomic<int> capture_state{-1};
std::atomic<int> media_error{0};
std::atomic<uint64_t> frame_count{0};
std::string message;
std::vector<uint8_t> qr;
int qr_edge = 0;
uint32_t qr_version = 0;
bool refresh = false;
std::atomic<int> target_display{0};
std::atomic<int> pointer_x{0};
std::atomic<int> pointer_y{0};
std::atomic<int> display_width{1};
std::atomic<int> display_height{1};
std::set<int> held_keys;
std::set<int> held_buttons;

void OnError(OH_AVCodec*, int32_t code, void*) { media_error.store(code ? code : -1); }
void OnFormat(OH_AVCodec*, OH_AVFormat*, void*) {}
void OnInput(OH_AVCodec*, uint32_t, OH_AVBuffer*, void*) {}
void OnOutput(OH_AVCodec* codec, uint32_t index, OH_AVBuffer* buffer, void*) {
  OH_AVCodecBufferAttr attr{};
  uint8_t* data = OH_AVBuffer_GetAddr(buffer);
  if (OH_AVBuffer_GetBufferAttr(buffer, &attr) == AV_ERR_OK && data &&
      attr.offset >= 0 && attr.size > 0 && static_cast<size_t>(attr.size) <= kMaxFrameBytes &&
      static_cast<int64_t>(attr.offset) + attr.size <= OH_AVBuffer_GetCapacity(buffer)) {
    std::lock_guard<std::mutex> lock(frame_mutex);
    if (attr.flags & AVCODEC_BUFFER_FLAGS_CODEC_DATA) {
      codec_data.assign(data + attr.offset, data + attr.offset + attr.size);
    } else {
      const bool idr = attr.flags & AVCODEC_BUFFER_FLAGS_SYNC_FRAME;
      if (frames.size() >= kMaxFrames) { frames.clear(); needs_idr = true; }
      if (!needs_idr || idr) {
        std::vector<uint8_t> bytes;
        if (idr) { bytes = codec_data; needs_idr = false; }
        bytes.insert(bytes.end(), data + attr.offset, data + attr.offset + attr.size);
        frames.push_back(std::move(bytes));
        const uint64_t count = frame_count.fetch_add(1) + 1;
        if (count <= 3) {
          OH_LOG_Print(LOG_APP, LOG_INFO, 0xFF00, "UUHarmony", "Encoded frame=%{public}llu bytes=%{public}d keyframe=%{public}d",
                       static_cast<unsigned long long>(count), attr.size, static_cast<int>(idr));
        }
      }
    }
  }
  OH_VideoEncoder_FreeOutputBuffer(codec, index);
}
void OnState(OH_AVScreenCapture* session, OH_AVScreenCaptureStateCode state, void*) {
  if (state == OH_SCREEN_CAPTURE_STATE_STARTED) { OH_AVScreenCapture_SetMaxVideoFrameRate(session, 30); }
  capture_state.store(static_cast<int>(state));
  accept_input.store(state == OH_SCREEN_CAPTURE_STATE_STARTED);
  OH_LOG_Print(LOG_APP, LOG_INFO, 0xFF00, "UUHarmony", "Capture state=%{public}d", static_cast<int>(state));
}
void OnCaptureError(OH_AVScreenCapture*, int32_t code, void*) { media_error.store(code ? code : -1); }
void StopMedia() {
  accept_input.store(false);
  if (capture) { OH_AVScreenCapture_StopScreenCapture(capture); OH_AVScreenCapture_Release(capture); capture = nullptr; }
  if (encoder) { OH_VideoEncoder_Stop(encoder); OH_VideoEncoder_Destroy(encoder); encoder = nullptr; }
  if (surface) { OH_NativeWindow_DestroyNativeWindow(surface); surface = nullptr; }
  std::lock_guard<std::mutex> lock(frame_mutex);
  frames.clear(); codec_data.clear(); needs_idr = true;
  capture_state.store(-1);
}
int KeyCode(unsigned long symbol) {
  if (symbol >= 'a' && symbol <= 'z') return KEYCODE_A + symbol - 'a';
  if (symbol >= 'A' && symbol <= 'Z') return KEYCODE_A + symbol - 'A';
  if (symbol >= '0' && symbol <= '9') return KEYCODE_0 + symbol - '0';
  if (symbol >= kFunctionFirst && symbol <= kFunctionLast) return KEYCODE_F1 + symbol - kFunctionFirst;
  if (symbol >= kKeypadDigitFirst && symbol <= kKeypadDigitLast) return KEYCODE_NUMPAD_0 + symbol - kKeypadDigitFirst;
  switch (symbol) {
    case kBackspace: return KEYCODE_DEL;
    case kTab: return KEYCODE_TAB;
    case kEnter: case kKeypadEnter: return KEYCODE_ENTER;
    case kEscape: return KEYCODE_ESCAPE;
    case kDelete: return KEYCODE_FORWARD_DEL;
    case kHome: return KEYCODE_MOVE_HOME;
    case kLeft: return KEYCODE_DPAD_LEFT;
    case kUp: return KEYCODE_DPAD_UP;
    case kRight: return KEYCODE_DPAD_RIGHT;
    case kDown: return KEYCODE_DPAD_DOWN;
    case kPageUp: return KEYCODE_PAGE_UP;
    case kPageDown: return KEYCODE_PAGE_DOWN;
    case kEnd: return KEYCODE_MOVE_END;
    case kInsert: return KEYCODE_INSERT;
    case kShiftLeft: return KEYCODE_SHIFT_LEFT;
    case kShiftRight: return KEYCODE_SHIFT_RIGHT;
    case kControlLeft: return KEYCODE_CTRL_LEFT;
    case kControlRight: return KEYCODE_CTRL_RIGHT;
    case kCapsLock: return KEYCODE_CAPS_LOCK;
    case kAltLeft: return KEYCODE_ALT_LEFT;
    case kAltRight: return KEYCODE_ALT_RIGHT;
    case kMetaLeft: return KEYCODE_META_LEFT;
    case kMetaRight: return KEYCODE_META_RIGHT;
    case kNumLock: return KEYCODE_NUM_LOCK;
    case kScrollLock: return KEYCODE_SCROLL_LOCK;
    case kPause: return KEYCODE_BREAK;
    case ' ': return KEYCODE_SPACE;
    case '-': return KEYCODE_MINUS;
    case '=': return KEYCODE_EQUALS;
    case '[': return KEYCODE_LEFT_BRACKET;
    case ']': return KEYCODE_RIGHT_BRACKET;
    case '\\': return KEYCODE_BACKSLASH;
    case ';': return KEYCODE_SEMICOLON;
    case '\'': return KEYCODE_APOSTROPHE;
    case ',': return KEYCODE_COMMA;
    case '.': return KEYCODE_PERIOD;
    case '/': return KEYCODE_SLASH;
    case '`': return KEYCODE_GRAVE;
    default: return -1;
  }
}
int SendKey(int key, bool down) {
  Input_KeyEvent* event = OH_Input_CreateKeyEvent();
  if (!event) return -1;
  OH_Input_SetKeyEventKeyCode(event, key);
  OH_Input_SetKeyEventDisplayId(event, target_display.load());
  OH_Input_SetKeyEventAction(event, down ? KEY_ACTION_DOWN : KEY_ACTION_UP);
  int code = OH_Input_InjectKeyEvent(event);
  OH_Input_DestroyKeyEvent(&event);
  return code;
}
int SendMouse(int action, int button, int axis = -1, float value = 0,
              int x = -1, int y = -1) {
  Input_MouseEvent* event = OH_Input_CreateMouseEvent();
  if (!event) return -1;
  OH_Input_SetMouseEventAction(event, action);
  OH_Input_SetMouseEventDisplayId(event, target_display.load());
  OH_Input_SetMouseEventDisplayX(event, x < 0 ? pointer_x.load() : x);
  OH_Input_SetMouseEventDisplayY(event, y < 0 ? pointer_y.load() : y);
  OH_Input_SetMouseEventButton(event, button);
  if (axis >= 0) { OH_Input_SetMouseEventAxisType(event, axis); OH_Input_SetMouseEventAxisValue(event, value); }
  int code = OH_Input_InjectMouseEvent(event);
  OH_Input_DestroyMouseEvent(&event);
  return code;
}


}  // namespace steamsir::ohos
using namespace steamsir::ohos;

extern "C" int uu_media_start(int width, int height) {
  std::lock_guard<std::mutex> lock(api_mutex);
  StopMedia(); media_error.store(0); frame_count.store(0);
  encoder = OH_VideoEncoder_CreateByMime("video/avc");
  if (!encoder) return -1;
  OH_AVCodecCallback callbacks{OnError, OnFormat, OnInput, OnOutput};
  int code = OH_VideoEncoder_RegisterCallback(encoder, callbacks, nullptr);
  OH_AVFormat* format = OH_AVFormat_Create();
  if (!format) { StopMedia(); return -1; }
  OH_AVFormat_SetIntValue(format, OH_MD_KEY_WIDTH, width);
  OH_AVFormat_SetIntValue(format, OH_MD_KEY_HEIGHT, height);
  OH_AVFormat_SetDoubleValue(format, OH_MD_KEY_FRAME_RATE, 30.0);
  OH_AVFormat_SetLongValue(format, OH_MD_KEY_BITRATE, 8000000);
  OH_AVFormat_SetIntValue(format, OH_MD_KEY_VIDEO_ENCODE_BITRATE_MODE, BITRATE_MODE_CBR);
  OH_AVFormat_SetIntValue(format, OH_MD_KEY_PIXEL_FORMAT, AV_PIXEL_FORMAT_SURFACE_FORMAT);
  OH_AVFormat_SetIntValue(format, OH_MD_KEY_I_FRAME_INTERVAL, 1000);
  OH_AVFormat_SetIntValue(format, OH_MD_KEY_PROFILE, AVC_PROFILE_BASELINE);
  OH_AVFormat_SetIntValue(format, OH_MD_KEY_VIDEO_ENCODER_ENABLE_B_FRAME, 0);
  if (code == AV_ERR_OK) code = OH_VideoEncoder_Configure(encoder, format);
  OH_AVFormat_Destroy(format);
  if (code == AV_ERR_OK) code = OH_VideoEncoder_GetSurface(encoder, &surface);
  if (code == AV_ERR_OK) code = OH_VideoEncoder_Prepare(encoder);
  if (code == AV_ERR_OK) code = OH_VideoEncoder_Start(encoder);
  if (code != AV_ERR_OK) { StopMedia(); return code; }
  capture = OH_AVScreenCapture_Create();
  if (!capture) { StopMedia(); return -1; }
  OH_AVScreenCaptureConfig config{};
  config.captureMode = OH_CAPTURE_HOME_SCREEN;
  config.dataType = OH_ORIGINAL_STREAM;
  config.videoInfo.videoCapInfo.videoFrameWidth = width;
  config.videoInfo.videoCapInfo.videoFrameHeight = height;
  config.videoInfo.videoCapInfo.videoSource = OH_VIDEO_SOURCE_SURFACE_RGBA;
  code = OH_AVScreenCapture_SetStateCallback(capture, OnState, nullptr);
  if (code == 0) code = OH_AVScreenCapture_SetErrorCallback(capture, OnCaptureError, nullptr);
  if (code == 0) code = OH_AVScreenCapture_SetMicrophoneEnabled(capture, false);
  if (code == 0) code = OH_AVScreenCapture_Init(capture, config);
  // The viewer draws its local pointer; embedding the host cursor doubles it.
  if (code == 0) code = OH_AVScreenCapture_ShowCursor(capture, false);
  if (code == 0) code = OH_AVScreenCapture_StartScreenCaptureWithSurface(capture, surface);
  if (code != 0) StopMedia();
  return code;
}
extern "C" void uu_media_stop() { std::lock_guard<std::mutex> lock(api_mutex); StopMedia(); }
extern "C" int uu_media_state() { return media_error.load() ? -2 : capture_state.load(); }
extern "C" uint64_t uu_media_frame_count() { return frame_count.load(); }
extern "C" int uu_video_config(uint32_t fps, uint32_t kbps) {
  std::lock_guard<std::mutex> lock(api_mutex);
  if (!encoder || capture_state.load() != OH_SCREEN_CAPTURE_STATE_STARTED) return -1;
  OH_AVFormat* format = OH_AVFormat_Create();
  if (!format) return -1;
  OH_AVFormat_SetLongValue(format, OH_MD_KEY_BITRATE, static_cast<int64_t>(std::clamp(kbps, 500u, 30000u)) * 1000);
  OH_AVFormat_SetIntValue(format, OH_MD_KEY_REQUEST_I_FRAME, 1);
  int code = OH_VideoEncoder_SetParameter(encoder, format);
  OH_AVFormat_Destroy(format);
  { std::lock_guard<std::mutex> guard(frame_mutex); frames.clear(); needs_idr = true; }
  return code;
}
extern "C" void uu_video_request_idr() {
  std::lock_guard<std::mutex> lock(api_mutex);
  if (!encoder) return;
  OH_AVFormat* format = OH_AVFormat_Create();
  if (!format) return;
  OH_AVFormat_SetIntValue(format, OH_MD_KEY_REQUEST_I_FRAME, 1);
  OH_VideoEncoder_SetParameter(encoder, format); OH_AVFormat_Destroy(format);
}
extern "C" int uu_video_poll(uint8_t* data, size_t capacity) {
  if (media_error.load()) return -2;
  if (capture_state.load() != OH_SCREEN_CAPTURE_STATE_STARTED) return -1;
  std::lock_guard<std::mutex> lock(frame_mutex);
  if (frames.empty()) return 0;
  if (frames.front().size() > capacity) return -3;
  int size = frames.front().size();
  std::memcpy(data, frames.front().data(), size); frames.pop_front(); return size;
}
extern "C" void uu_platform_status(const char* text) { std::lock_guard<std::mutex> lock(ui_mutex); message = text ? text : ""; }
extern "C" int uu_login_ui_start(const char*, const char*, const char*, const char*) { return 0; }
extern "C" void uu_login_ui_set_qr(const uint8_t* rgba, int edge) {
  if (!rgba || edge < 1 || edge > 2048) return;
  std::lock_guard<std::mutex> lock(ui_mutex); qr.assign(rgba, rgba + edge * edge * 4); qr_edge = edge; ++qr_version;
}
extern "C" void uu_login_ui_set_status(const char* text, int) { uu_platform_status(text); }
extern "C" void uu_login_ui_set_done(const char* text) { uu_platform_status(text); }
extern "C" int uu_login_ui_closed() { return 0; }
extern "C" void uu_login_ui_close() {}
extern "C" void uu_login_ui_offer_refresh(int) {}
extern "C" int uu_login_ui_take_refresh() { std::lock_guard<std::mutex> lock(ui_mutex); bool value = refresh; refresh = false; return value; }
extern "C" int x11_terminal_shortcut_state() { return 0; }
extern "C" int x11_type_utf8(const char*) { return -1; }
extern "C" int x11_pointer_state(int*, int*, unsigned*) { return -1; }
extern "C" int x11_key_event(int, int) { return -1; }
extern "C" int x11_key_symbol(unsigned long symbol, int down) {
  if (down && !accept_input.load()) return -1;
  const int key = KeyCode(symbol); if (key < 0) return -1;
  std::lock_guard<std::mutex> lock(input_mutex);
  if (!down && !held_keys.count(key)) return 0;
  int code = SendKey(key, down != 0);
  if (code == 0) { if (down) held_keys.insert(key); else held_keys.erase(key); }
  return code;
}
extern "C" int x11_mouse_move(int x, int y) {
  if (!accept_input.load()) return -1;
  x = std::clamp(x, 0, display_width.load() - 1);
  y = std::clamp(y, 0, display_height.load() - 1);
  int code = SendMouse(MOUSE_ACTION_MOVE, MOUSE_BUTTON_NONE, -1, 0, x, y);
  if (code == 0) { pointer_x.store(x); pointer_y.store(y); }
  return code;
}
extern "C" int x11_mouse_relative(int dx, int dy) {
  return x11_mouse_move(pointer_x.load() + dx, pointer_y.load() + dy);
}
extern "C" int x11_mouse_button(int button, int down) {
  if (down && !accept_input.load()) return -1;
  std::lock_guard<std::mutex> lock(input_mutex);
  if (button >= 4 && button <= 7) {
    if (!down) return 0;
    const int axis = button <= 5 ? MOUSE_AXIS_SCROLL_VERTICAL : MOUSE_AXIS_SCROLL_HORIZONTAL;
    const float value = (button == 4 || button == 7) ? -1 : 1;
    int code = SendMouse(MOUSE_ACTION_AXIS_BEGIN, MOUSE_BUTTON_NONE, axis, value);
    if (code == 0) code = SendMouse(MOUSE_ACTION_AXIS_END, MOUSE_BUTTON_NONE, axis, 0);
    return code;
  }
  const int mapped = button == 1 ? MOUSE_BUTTON_LEFT : button == 2 ? MOUSE_BUTTON_MIDDLE : button == 3 ? MOUSE_BUTTON_RIGHT : MOUSE_BUTTON_NONE;
  if (mapped == MOUSE_BUTTON_NONE) return -1;
  if (!down && !held_buttons.count(mapped)) return 0;
  int code = SendMouse(down ? MOUSE_ACTION_BUTTON_DOWN : MOUSE_ACTION_BUTTON_UP, mapped);
  if (code == 0) { if (down) held_buttons.insert(mapped); else held_buttons.erase(mapped); }
  return code;
}
extern "C" int x11_release_all() {
  std::lock_guard<std::mutex> lock(input_mutex);
  for (int key : held_keys) SendKey(key, false);
  for (int button : held_buttons) SendMouse(MOUSE_ACTION_BUTTON_UP, button);
  held_keys.clear(); held_buttons.clear(); return 0;
}

extern "C" bool uu_input_authorized() {
  Input_InjectionStatus state = UNAUTHORIZED;
  return OH_Input_QueryAuthorizedStatus(&state) == INPUT_SUCCESS && state == AUTHORIZED;
}

extern "C" void uu_login_clear() {
  std::lock_guard<std::mutex> lock(ui_mutex);
  std::fill(qr.begin(), qr.end(), 0); qr.clear(); qr_edge = 0; ++qr_version;
}
extern "C" void uu_platform_log(int level, const char* text) {
  const LogLevel priority = level == 1 ? LOG_ERROR : level == 2 ? LOG_WARN : LOG_INFO;
  OH_LOG_Print(LOG_APP, priority, 0xFF00, "UUHarmony", "%{public}s", text ? text : "");
}

extern "C" void uu_input_set_display(int display, int width, int height) {
  target_display.store(display); display_width.store(width); display_height.store(height);
  pointer_x.store(width / 2); pointer_y.store(height / 2);
}

namespace steamsir::ohos {
// Matches the existing GTK UI command boundary consumed by assist::LocalUi.
enum class AssistanceCommand : int { kAllow = 1, kDeny = 2, kReset = 3 };
std::string assistance_id;
std::string assistance_code;
bool assistance_allowed = false;
std::deque<AssistanceCommand> assistance_commands;
constexpr size_t kMaxAssistanceCommands = 8;
}  // namespace steamsir::ohos

extern "C" int uu_assist_ui_start(int, int) {
  std::lock_guard<std::mutex> lock(ui_mutex);
  assistance_commands.clear();
  assistance_id.clear();
  std::fill(assistance_code.begin(), assistance_code.end(), 0);
  assistance_code.clear();
  assistance_allowed = false;
  return 0;
}
extern "C" void uu_assist_ui_present() {}
extern "C" void uu_ui_set_user(const char*) {}
extern "C" void uu_assist_ui_set(const char* id, const char* code, int allowed,
                                   int, int, int) {
  std::lock_guard<std::mutex> lock(ui_mutex);
  assistance_id = id ? id : "";
  std::fill(assistance_code.begin(), assistance_code.end(), 0);
  assistance_code = code ? code : "";
  assistance_allowed = allowed != 0;
}
extern "C" int uu_assist_ui_take_command(char* argument, size_t capacity) {
  if (argument && capacity) argument[0] = '\0';
  std::lock_guard<std::mutex> lock(ui_mutex);
  if (assistance_commands.empty()) return 0;
  const int command = static_cast<int>(assistance_commands.front());
  assistance_commands.pop_front();
  return command;
}
extern "C" void uu_assist_ui_close() {
  std::lock_guard<std::mutex> lock(ui_mutex);
  assistance_id.clear();
  std::fill(assistance_code.begin(), assistance_code.end(), 0);
  assistance_code.clear();
  assistance_allowed = false;
  assistance_commands.clear();
}
extern "C" int uu_ui_set_problem(const char*, const char*, int) { return -1; }
extern "C" int uu_assist_set_enabled(bool enabled) {
  std::lock_guard<std::mutex> lock(ui_mutex);
  if (assistance_id.empty() || assistance_commands.size() >= kMaxAssistanceCommands) return -1;
  assistance_commands.push_back(enabled ? AssistanceCommand::kAllow : AssistanceCommand::kDeny);
  return 0;
}
extern "C" int uu_assist_reset_code() {
  std::lock_guard<std::mutex> lock(ui_mutex);
  if (assistance_id.empty() || assistance_commands.size() >= kMaxAssistanceCommands) return -1;
  assistance_commands.push_back(AssistanceCommand::kReset);
  return 0;
}
