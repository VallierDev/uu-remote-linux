# Run with /usr/bin/python, UU_TEST_BIN set to the test binary, and GDK_BACKEND=x11 or wayland.
# Opens and focuses a dedicated entry; only the built-in sample is injected.
import os
os.environ["GTK_IM_MODULE"]="gtk-im-context-simple"
import gi,subprocess,threading
gi.require_version('Gtk','3.0')
from gi.repository import Gtk,GLib
events=[]
passed=False
expected=os.environ.get("UU_PROBE_SEQUENCE","abc你好XYZ").replace("|","")
w=Gtk.Window(title='UU input acceptance'); e=Gtk.Entry();w.add(e);w.show_all();e.grab_focus();w.present();e.connect("key-press-event",lambda widget,event:events.append(event.keyval) and False)
def start():
 if "UU input acceptance" not in subprocess.check_output(["niri","msg","-j","focused-window"],text=True):
  Gtk.main_quit();return False
 def run():
  r=subprocess.run([os.environ.get('UU_TEST_BIN',os.path.expanduser('~/.local/bin/uuremote-server')),'wayland-text-probe'],env={**os.environ,'UU_PROBE_TEXT':'abc你好XYZ'},stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
  GLib.timeout_add(1000,finish,r.returncode)
 threading.Thread(target=run).start();return False
def finish(code):
 global passed
 passed=code==0 and e.get_text()==expected
 print('probe_exit=',code,'matches=',e.get_text()==expected,'received_chars=',len(e.get_text()),'key_events=',len(events),flush=True);Gtk.main_quit();return False
GLib.timeout_add(1500,start);GLib.timeout_add(15000,lambda:(Gtk.main_quit(),False)[1]);Gtk.main()

raise SystemExit(0 if passed else 1)
