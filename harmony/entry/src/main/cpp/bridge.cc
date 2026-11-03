#include <napi/native_api.h>
#include <multimodalinput/oh_input_manager.h>
#include <cstdint>
#include <cstring>
#include <mutex>
#include <string>
#include <vector>
extern "C" {
int uu_host_init(const char*, uint32_t, uint32_t, const char*);
int uu_host_login();
bool uu_host_logged_in();
int uu_host_start();
void uu_host_stop();
int x11_release_all();
int uu_media_start(int, int);
void uu_input_set_display(int, int, int);
void uu_media_stop();
int uu_media_state();
uint64_t uu_media_frame_count();
int uu_assist_set_enabled(bool);
int uu_assist_reset_code();
}
namespace steamsir::ohos {
extern std::mutex ui_mutex;
extern std::string message;
extern std::vector<uint8_t> qr;
extern int qr_edge;
extern uint32_t qr_version;
extern bool refresh;
extern std::string assistance_id;
extern std::string assistance_code;
extern bool assistance_allowed;

napi_value Integer(napi_env env, int64_t value) {
  napi_value result; napi_create_int64(env, value, &result); return result;
}
napi_value Initialize(napi_env env, napi_callback_info info) {
  napi_value args[4]; size_t count = 4;
  napi_get_cb_info(env, info, &count, args, nullptr, nullptr);
  if (count != 4) { napi_throw_type_error(env, nullptr, "Expected config path, display dimensions and language"); return nullptr; }
  size_t length = 0;
  if (napi_get_value_string_utf8(env, args[0], nullptr, 0, &length) != napi_ok || length > 4096) return Integer(env, -1);
  std::vector<char> path(length + 1);
  uint32_t width = 0, height = 0;
  char language[3] = {};
  size_t language_length = 0;
  if (napi_get_value_string_utf8(env, args[0], path.data(), path.size(), &length) != napi_ok ||
      napi_get_value_uint32(env, args[1], &width) != napi_ok || napi_get_value_uint32(env, args[2], &height) != napi_ok ||
      napi_get_value_string_utf8(env, args[3], nullptr, 0, &language_length) != napi_ok || language_length != 2 ||
      napi_get_value_string_utf8(env, args[3], language, sizeof(language), &language_length) != napi_ok) return Integer(env, -1);
  return Integer(env, uu_host_init(path.data(), width, height, language));
}
napi_value Login(napi_env env, napi_callback_info) { return Integer(env, uu_host_login()); }
napi_value StartHost(napi_env env, napi_callback_info) { return Integer(env, uu_host_start()); }
napi_value Stop(napi_env env, napi_callback_info) {
  uu_host_stop();
  uu_media_stop();
  x11_release_all();
  // Ending a sharing session releases input without revoking the user's grant.
  return Integer(env, 0);
}
napi_value StartCapture(napi_env env, napi_callback_info info) {
  napi_value args[3]; size_t count = 3; napi_get_cb_info(env, info, &count, args, nullptr, nullptr);
  int32_t width = 0, height = 0, display_id = 0;
  if (count != 3 || napi_get_value_int32(env, args[0], &width) != napi_ok ||
      napi_get_value_int32(env, args[1], &height) != napi_ok ||
      napi_get_value_int32(env, args[2], &display_id) != napi_ok || display_id < 0 || width < 1 || height < 1 || width > 8192 || height > 8192) return Integer(env, -1);
  uu_input_set_display(display_id, width, height);
  return Integer(env, uu_media_start(width, height));
}
void OnAuthorized(Input_InjectionStatus) {}
napi_value RequestInput(napi_env env, napi_callback_info) {
  Input_InjectionStatus authorization = UNAUTHORIZED;
  Input_Result result = OH_Input_QueryAuthorizedStatus(&authorization);
  if (result != INPUT_SUCCESS) return Integer(env, result);
  if (authorization == AUTHORIZED || authorization == AUTHORIZING) {
    return Integer(env, INPUT_SUCCESS);
  }
  result = OH_Input_RequestInjection(OnAuthorized);
  if (result == INPUT_INJECTION_AUTHORIZED ||
      result == INPUT_INJECTION_AUTHORIZING) {
    return Integer(env, INPUT_SUCCESS);
  }
  return Integer(env, result);
}
napi_value Refresh(napi_env env, napi_callback_info) {
  std::lock_guard<std::mutex> lock(ui_mutex); refresh = true; return Integer(env, 0);
}
napi_value State(napi_env env, napi_callback_info) {
  napi_value out, value; napi_create_object(env, &out);
  { std::lock_guard<std::mutex> lock(ui_mutex);
    napi_create_string_utf8(env, message.c_str(), message.size(), &value); napi_set_named_property(env, out, "message", value);
    napi_set_named_property(env, out, "qrVersion", Integer(env, qr_version));
    napi_set_named_property(env, out, "qrEdge", Integer(env, qr_edge));
    napi_create_string_utf8(env, assistance_id.c_str(), assistance_id.size(), &value);
    napi_set_named_property(env, out, "assistanceDeviceId", value);
    napi_create_string_utf8(env, assistance_code.c_str(), assistance_code.size(), &value);
    napi_set_named_property(env, out, "assistanceCode", value);
    napi_get_boolean(env, assistance_allowed, &value);
    napi_set_named_property(env, out, "assistanceEnabled", value);
  }
  napi_get_boolean(env, uu_host_logged_in(), &value); napi_set_named_property(env, out, "loggedIn", value);
  napi_set_named_property(env, out, "captureState", Integer(env, uu_media_state()));
  napi_set_named_property(env, out, "frames", Integer(env, uu_media_frame_count()));
  Input_InjectionStatus permission = UNAUTHORIZED;
  OH_Input_QueryAuthorizedStatus(&permission);
  napi_set_named_property(env, out, "inputState", Integer(env, permission));
  return out;
}
napi_value Qr(napi_env env, napi_callback_info) {
  std::lock_guard<std::mutex> lock(ui_mutex);
  napi_value result; void* data = nullptr;
  if (napi_create_arraybuffer(env, qr.size(), &data, &result) != napi_ok) return nullptr;
  if (!qr.empty()) std::memcpy(data, qr.data(), qr.size()); return result;
}
napi_value SetAssistanceEnabled(napi_env env, napi_callback_info info) {
  napi_value argument;
  size_t count = 1;
  bool enabled = false;
  napi_get_cb_info(env, info, &count, &argument, nullptr, nullptr);
  if (count != 1 || napi_get_value_bool(env, argument, &enabled) != napi_ok) return Integer(env, -1);
  return Integer(env, uu_assist_set_enabled(enabled));
}
napi_value ResetAssistanceCode(napi_env env, napi_callback_info) {
  return Integer(env, uu_assist_reset_code());
}
napi_value Register(napi_env env, napi_value exports) {
  napi_property_descriptor properties[] = {
    {"initialize", nullptr, Initialize, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"beginLogin", nullptr, Login, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"refreshQr", nullptr, Refresh, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"startHost", nullptr, StartHost, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"startCapture", nullptr, StartCapture, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"stop", nullptr, Stop, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"requestInput", nullptr, RequestInput, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"getState", nullptr, State, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"setAssistanceEnabled", nullptr, SetAssistanceEnabled, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"resetAssistanceCode", nullptr, ResetAssistanceCode, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"getQrPixels", nullptr, Qr, nullptr, nullptr, nullptr, napi_default, nullptr},
  };
  napi_define_properties(env, exports, sizeof(properties) / sizeof(properties[0]), properties); return exports;
}
}  // namespace steamsir::ohos
static napi_module module = {1, 0, nullptr, steamsir::ohos::Register, "entry", nullptr, {0}};
extern "C" __attribute__((constructor)) void RegisterEntry() { napi_module_register(&module); }
