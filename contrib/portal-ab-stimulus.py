#!/usr/bin/env python3
"""Local synthetic desktop workload. No screen capture, input logging or network.
Control through --directory/control.json: mode=idle|static|motion|quit, run=id.
Escape hides the window. status.json contains only counters and dimensions.
"""
import argparse
import json
import math
import time
from pathlib import Path
import gi
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, GLib, Gdk

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--directory', type=Path, required=True)
args = parser.parse_args()
root = args.directory
mode, run, ticks, draws = 'idle', None, 0, 0
phase_start = time.monotonic()
last_status = 0
window = Gtk.Window(title='UU performance test / 性能测试')
window.set_default_size(1280, 720)
area = Gtk.Fixed()
window.add(area)
window.connect('destroy', Gtk.main_quit)

def color(widget, value):
    css=Gtk.CssProvider()
    css.load_from_data((' * { background-color: '+value+'; }').encode())
    widget.get_style_context().add_provider(css,Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
color(window,'#101726')
stripes=[]
for row in range(8):
    box=Gtk.EventBox(); color(box,'#285279');area.put(box,0,0);stripes.append(box)
boxes=[]
for i in range(12):
    box=Gtk.EventBox();box.set_size_request(48,48);color(box,'#e68a26');area.put(box,0,0);boxes.append(box)
label=Gtk.Label(label='UU synthetic workload - 30 Hz - Escape to hide')
label.set_markup('<span foreground="white" size="18000" weight="bold">UU synthetic workload - 30 Hz - Escape to hide</span>')
area.put(label,40,20)
last_size=None

def on_key(_, event):
    if event.keyval == Gdk.KEY_Escape:window.hide()
    return False
window.connect('key-press-event',on_key)

def update_scene():
    global draws
    w,h=area.get_allocated_width(),area.get_allocated_height()
    if w<100 or h<100:return
    phase=(ticks%300)/300 if mode=='motion' else 0
    for i,box in enumerate(boxes):
        angle=phase*2*math.pi+i*math.pi/6
        area.move(box,int(w*.5+w*.32*math.cos(angle)-24),int(h*.5+h*.32*math.sin(angle)-24))
    draws+=1

def size_changed(_,allocation):
    global last_size
    size=(allocation.width,allocation.height)
    if size==last_size:return
    last_size=size
    w,h=size
    for row,box in enumerate(stripes):
        box.set_size_request(int(w*.84),int(h*.045));area.move(box,int(w*.08),int(h*(.12+row*.09)))
    update_scene()
area.connect('size-allocate',size_changed)

def tick():
    global mode, run, ticks, draws, phase_start, last_status
    now = time.monotonic()
    try:
        control = json.loads((root/'control.json').read_text())
    except (OSError, ValueError):
        control = {'mode': 'idle', 'run': None}
    requested = control.get('mode', 'idle')
    if requested == 'quit':
        Gtk.main_quit(); return False
    if (requested, control.get('run')) != (mode, run):
        mode, run, ticks, draws = requested, control.get('run'), 0, 0
        phase_start = now
        if mode in ('static', 'motion'):
            window.fullscreen(); window.show_all(); window.present(); update_scene()
        else:
            window.hide()
    if mode == 'motion':
        step = int((now-phase_start)*30)
        if step != ticks:
            ticks = step; update_scene()
    if now-last_status >= 1:
        status = dict(mode=mode, run=run, ticks=ticks, draws=draws,
                      elapsed=now-phase_start, visible=window.get_visible(),
                      width=area.get_allocated_width(), height=area.get_allocated_height())
        temporary = root/'status.tmp'
        temporary.write_text(json.dumps(status))
        temporary.replace(root/'status.json')
        last_status = now
    return True
GLib.timeout_add(8, tick)
Gtk.main()
