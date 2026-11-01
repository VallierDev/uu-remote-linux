# Requires an unlocked niri session; opens a fullscreen test window and moves the pointer.
# UU_TEST_OUTPUT selects the output. UU_TEST_ENGINE is the built libuu-wayland.so.
import os,json,subprocess,ctypes,gi
os.environ['UU_WAYLAND_OUTPUT']=os.environ['UU_TEST_OUTPUT']
gi.require_version('Gtk','3.0');from gi.repository import Gtk,Gdk,GLib
inv=json.loads(subprocess.check_output(['niri','msg','-j','outputs']))
out=next(o for o in inv.values() if o['name']==os.environ['UU_TEST_OUTPUT']);geo=out['logical'];d=Gdk.Display.get_default()
monitor=next(i for i in range(d.get_n_monitors()) if (d.get_monitor(i).get_geometry().x,d.get_monitor(i).get_geometry().y)==(geo['x'],geo['y']))
w=Gtk.Window(title='UU pointer acceptance');box=Gtk.EventBox();box.set_above_child(True);grid=Gtk.Grid();grid.set_row_homogeneous(True);grid.set_column_homogeneous(True)
for i,color in enumerate(['red','lime','blue','yellow']):
 cell=Gtk.EventBox();rgba=Gdk.RGBA();rgba.parse(color);cell.override_background_color(Gtk.StateFlags.NORMAL,rgba);grid.attach(cell,i%2,i//2,1,1)
box.add(grid);w.add(box);box.add_events(Gdk.EventMask.POINTER_MOTION_MASK | Gdk.EventMask.ENTER_NOTIFY_MASK)
positions=[];box.connect('motion-notify-event',lambda widget,e:positions.append((e.x,e.y)) or False)
box.connect('enter-notify-event',lambda widget,e:positions.append((e.x,e.y)) or False)
w.fullscreen_on_monitor(w.get_screen(),monitor);w.show_all()
lib=ctypes.CDLL(os.environ['UU_TEST_ENGINE']);width=ctypes.c_int();height=ctypes.c_int();points=iter([(0.2,0.2),(0.8,0.2),(0.8,0.8),(0.2,0.8),(0.5,0.5)]);passed=[];image_matches=0
def step():
 try:x,y=next(points)
 except StopIteration:print('pointer_points_passed=',sum(passed),'total=',len(passed),'points=',passed,flush=True);Gtk.main_quit();return False
 positions.clear();assert lib.uu_wl_move(int(x*width.value),int(y*height.value),0)==0
 def check():
  size=box.get_allocation();matched=any(abs(px-x*size.width)<4 and abs(py-y*size.height)<4 for px,py in positions);passed.append(matched);GLib.timeout_add(100,step);return False
 GLib.timeout_add(150,check);return False
def start():
 global image_matches
 assert lib.uu_wl_init(ctypes.byref(width),ctypes.byref(height))==0
 frame=(ctypes.c_ubyte*(width.value*height.value*4))();assert lib.uu_wl_capture(frame,len(frame))==0
 for (x,y),(r,g,b) in zip([(0.25,0.25),(0.75,0.25),(0.25,0.75),(0.75,0.75)],[(255,0,0),(0,255,0),(0,0,255),(255,255,0)]):
  at=(int(y*height.value)*width.value+int(x*width.value))*4
  image_matches+=abs(frame[at]-b)<10 and abs(frame[at+1]-g)<10 and abs(frame[at+2]-r)<10
 print('rotation_color_checks=',image_matches,'of=4',flush=True)
 step();return False
GLib.timeout_add(1500,start);GLib.timeout_add(10000,lambda:(Gtk.main_quit(),False)[1]);Gtk.main()
raise SystemExit(0 if len(passed)==5 and all(passed) and image_matches==4 else 1)
