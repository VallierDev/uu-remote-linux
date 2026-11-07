// Tests the production adapter with test-owned OH_Input events. No device ABI
// layout is assumed: only these fake C API functions access the fake objects.
#include <multimodalinput/oh_input_manager.h>

#include <atomic>
#include <cassert>
#include <cstdio>
#include <vector>

struct Input_MouseEvent {
  int action = 0;
  int button = MOUSE_BUTTON_NONE;
  int x = 0;
  int y = 0;
  int display = 0;
  int axis = -1;
  float value = 0;
};
struct Input_KeyEvent {};

namespace {
std::vector<Input_MouseEvent> events;
int next_result = INPUT_SUCCESS;
constexpr int kInjectedFailure = -10;
constexpr int kNoFailingAction = -1;
int failing_action = kNoFailingAction;
constexpr int kX11LeftButton = 1;
constexpr int kX11RightButton = 3;
constexpr int kX11WheelUpButton = 4;
constexpr int kX11WheelDownButton = 5;
constexpr int kX11WheelLeftButton = 6;
constexpr int kX11WheelRightButton = 7;
}  // namespace

extern "C" {
Input_MouseEvent* OH_Input_CreateMouseEvent() { return new Input_MouseEvent; }
void OH_Input_DestroyMouseEvent(Input_MouseEvent** event) {
  delete *event;
  *event = nullptr;
}
void OH_Input_SetMouseEventAction(Input_MouseEvent* event, int32_t value) { event->action = value; }
void OH_Input_SetMouseEventButton(Input_MouseEvent* event, int32_t value) { event->button = value; }
void OH_Input_SetMouseEventDisplayX(Input_MouseEvent* event, int32_t value) { event->x = value; }
void OH_Input_SetMouseEventDisplayY(Input_MouseEvent* event, int32_t value) { event->y = value; }
void OH_Input_SetMouseEventDisplayId(Input_MouseEvent* event, int32_t value) { event->display = value; }
void OH_Input_SetMouseEventAxisType(Input_MouseEvent* event, int32_t value) { event->axis = value; }
void OH_Input_SetMouseEventAxisValue(Input_MouseEvent* event, float value) { event->value = value; }
int32_t OH_Input_InjectMouseEvent(const Input_MouseEvent* event) {
  events.push_back(*event);
  if (event->action == failing_action) {
    failing_action = kNoFailingAction;
    return kInjectedFailure;
  }
  int result = next_result;
  next_result = INPUT_SUCCESS;
  return result;
}
Input_KeyEvent* OH_Input_CreateKeyEvent() { return new Input_KeyEvent; }
void OH_Input_DestroyKeyEvent(Input_KeyEvent** event) { delete *event; *event = nullptr; }
void OH_Input_SetKeyEventKeyCode(Input_KeyEvent*, int32_t) {}
void OH_Input_SetKeyEventDisplayId(Input_KeyEvent*, int32_t) {}
void OH_Input_SetKeyEventAction(Input_KeyEvent*, int32_t) {}
int32_t OH_Input_InjectKeyEvent(const Input_KeyEvent*) { return INPUT_SUCCESS; }

int x11_mouse_move(int x, int y);
int x11_mouse_relative(int dx, int dy);
int x11_mouse_button(int button, int down);
int x11_release_all();
}  // extern "C"

namespace steamsir::ohos {
extern std::atomic<bool> accept_input;
extern std::atomic<int> display_width;
extern std::atomic<int> display_height;
extern std::atomic<int> target_display;
}  // namespace steamsir::ohos

int main() {
  using namespace steamsir::ohos;
  accept_input.store(true);
  display_width.store(1920);
  display_height.store(1080);
  target_display.store(7);

  assert(x11_mouse_move(100, 200) == INPUT_SUCCESS);
  assert(events.back().button == MOUSE_BUTTON_NONE);
  assert(events.back().display == 7);
  assert(x11_mouse_button(kX11LeftButton, 1) == INPUT_SUCCESS);
  assert(events.back().action == MOUSE_ACTION_BUTTON_DOWN);
  assert(x11_mouse_move(300, 400) == INPUT_SUCCESS);
  assert(events.back().action == MOUSE_ACTION_MOVE);
  assert(events.back().button == MOUSE_BUTTON_LEFT);
  assert(x11_mouse_relative(20, -30) == INPUT_SUCCESS);
  assert(events.back().button == MOUSE_BUTTON_LEFT);
  assert(events.back().x == 320 && events.back().y == 370);
  assert(x11_mouse_button(kX11LeftButton, 0) == INPUT_SUCCESS);
  assert(events.back().action == MOUSE_ACTION_BUTTON_UP);
  assert(x11_mouse_move(350, 450) == INPUT_SUCCESS);
  assert(events.back().button == MOUSE_BUTTON_NONE);

  // Left remains the drag button even with another button held. Cleanup must
  // release both buttons and stop tagging subsequent moves as drags.
  assert(x11_mouse_button(kX11RightButton, 1) == INPUT_SUCCESS);
  assert(x11_mouse_move(350, 450) == INPUT_SUCCESS);
  assert(events.back().button == MOUSE_BUTTON_RIGHT);
  assert(x11_mouse_button(kX11LeftButton, 1) == INPUT_SUCCESS);
  assert(x11_mouse_relative(1, 1) == INPUT_SUCCESS);
  assert(events.back().button == MOUSE_BUTTON_LEFT);
  size_t before = events.size();
  assert(x11_release_all() == INPUT_SUCCESS);
  assert(events.size() == before + 2);
  assert(events[before].action == MOUSE_ACTION_BUTTON_UP);
  assert(events[before + 1].action == MOUSE_ACTION_BUTTON_UP);
  assert(x11_mouse_move(-100, 3000) == INPUT_SUCCESS);
  assert(events.back().button == MOUSE_BUTTON_NONE);
  assert(events.back().x == 0 && events.back().y == 1079);

  // A rejected press must not turn later moves into drags. A rejected move must
  // not change the origin used by the next relative event.
  next_result = kInjectedFailure;
  assert(x11_mouse_button(kX11LeftButton, 1) == kInjectedFailure);
  assert(x11_mouse_move(100, 100) == INPUT_SUCCESS);
  assert(events.back().button == MOUSE_BUTTON_NONE);
  next_result = kInjectedFailure;
  assert(x11_mouse_move(500, 500) == kInjectedFailure);
  assert(x11_mouse_relative(10, 20) == INPUT_SUCCESS);
  assert(events.back().x == 110 && events.back().y == 120);

  struct WheelCase {
    int button;
    int axis;
    float value;
  };
  const WheelCase wheels[] = {
      {kX11WheelUpButton, MOUSE_AXIS_SCROLL_VERTICAL, -45},
      {kX11WheelDownButton, MOUSE_AXIS_SCROLL_VERTICAL, 45},
      {kX11WheelLeftButton, MOUSE_AXIS_SCROLL_HORIZONTAL, -45},
      {kX11WheelRightButton, MOUSE_AXIS_SCROLL_HORIZONTAL, 45},
  };
  for (const auto& wheel : wheels) {
    before = events.size();
    assert(x11_mouse_button(wheel.button, 1) == INPUT_SUCCESS);
    assert(x11_mouse_button(wheel.button, 0) == INPUT_SUCCESS);
    assert(events.size() == before + 3);
    assert(events[before].action == MOUSE_ACTION_AXIS_BEGIN);
    assert(events[before].value == wheel.value);
    assert(events[before + 1].action == MOUSE_ACTION_AXIS_UPDATE);
    assert(events[before + 1].value == wheel.value);
    assert(events[before + 2].action == MOUSE_ACTION_AXIS_END);
    assert(events[before + 2].value == 0);
    for (size_t i = before; i < events.size(); ++i) {
      assert(events[i].button == MOUSE_BUTTON_NONE);
      assert(events[i].axis == wheel.axis);
      assert(events[i].x == 110 && events[i].y == 120);
      assert(events[i].display == 7);
    }
  }
  // End a started gesture even if UPDATE fails. Never send UPDATE after a
  // rejected BEGIN, and preserve held buttons across unrelated wheel events.
  failing_action = MOUSE_ACTION_AXIS_UPDATE;
  before = events.size();
  assert(x11_mouse_button(kX11WheelDownButton, 1) == kInjectedFailure);
  assert(events.size() == before + 3);
  assert(events.back().action == MOUSE_ACTION_AXIS_END);
  failing_action = MOUSE_ACTION_AXIS_BEGIN;
  before = events.size();
  assert(x11_mouse_button(kX11WheelDownButton, 1) == kInjectedFailure);
  assert(events.size() == before + 1);
  assert(x11_mouse_button(kX11LeftButton, 1) == INPUT_SUCCESS);
  assert(x11_mouse_button(kX11WheelDownButton, 1) == INPUT_SUCCESS);
  assert(x11_mouse_relative(0, 0) == INPUT_SUCCESS);
  assert(events.back().button == MOUSE_BUTTON_LEFT);
  assert(x11_mouse_button(kX11LeftButton, 0) == INPUT_SUCCESS);

  // Stopping capture rejects new input but still permits releasing our buttons.
  assert(x11_mouse_button(kX11LeftButton, 1) == INPUT_SUCCESS);
  accept_input.store(false);
  before = events.size();
  assert(x11_mouse_move(100, 200) != INPUT_SUCCESS);
  assert(x11_mouse_relative(10, 10) != INPUT_SUCCESS);
  assert(x11_mouse_button(kX11RightButton, 1) != INPUT_SUCCESS);
  assert(x11_mouse_button(kX11WheelDownButton, 1) != INPUT_SUCCESS);
  assert(x11_mouse_button(kX11WheelDownButton, 0) == INPUT_SUCCESS);
  assert(events.size() == before);
  assert(x11_release_all() == INPUT_SUCCESS);
  assert(events.size() == before + 1);
  assert(events.back().action == MOUSE_ACTION_BUTTON_UP);
  assert(events.back().button == MOUSE_BUTTON_LEFT);
  std::puts("HarmonyOS pointer event sequences passed");
}
