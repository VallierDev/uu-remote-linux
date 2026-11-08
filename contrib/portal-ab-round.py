#!/usr/bin/env python3
"""Run two controlled workload phases against an already connected service.
Stores numerical measurements only; does not switch services or capture pixels.
"""
import argparse, datetime, json, os, re, subprocess, sys, time
from pathlib import Path
p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--directory',type=Path,required=True)
p.add_argument('--round',required=True)
p.add_argument('--path',choices=['new','old'],required=True)
p.add_argument('--seconds',type=int,default=60)
a=p.parse_args();root=a.directory

def control(mode):
 t=root/'control.tmp';t.write_text(json.dumps({'mode':mode,'run':a.round+'-'+mode}));t.replace(root/'control.json')

def status():return json.loads((root/'status.json').read_text())

def snapshot():
 pid=int(subprocess.check_output(['systemctl','--user','show','uuremote-native.service','-p','MainPID','--value'],text=True))
 env=dict(x.split('=',1) for x in Path('/proc',str(pid),'environ').read_text().split('\0') if '=' in x)
 video=[]; audio=0
 children=set()
 for task in Path('/proc',str(pid),'task').iterdir():
  try:children.update((task/'children').read_text().split())
  except FileNotFoundError:pass
 for child in sorted(children):
  try: args=Path('/proc',child,'cmdline').read_bytes().decode().split('\0')
  except FileNotFoundError:continue
  if 'h264_nvenc' in args:
   keys=['-video_size','-framerate','-c:v','-g','-cq','-b:v','-maxrate','-bufsize','-preset','-tune','-pix_fmt']
   video.append({'pid':int(child),**{key:args[args.index(key)+1] for key in keys if key in args}})
  if 'libopus' in args:audio+=1
 expected='inproc' if a.path=='new' else 'process'
 assert env.get('UU_VIDEO_PIPELINE')==expected and env.get('UU_VIDEO_GOP_SECONDS')=='2'
 assert (len(video)==0 if a.path=='new' else len(video)==1), 'video process count mismatch'
 return dict(child_discovery='all_threads',pid=pid,pipeline=expected,gop_seconds=2,video_children=video,audio_children=audio)

try:
 metadata=snapshot()
 for mode in ['static','motion']:
  control(mode);time.sleep(10)
  begin=status(); assert begin['visible'] and begin['mode']==mode and begin['draws']>0
  path=root/(a.round+'-'+mode+'.json')
  with path.open('x') as out:
   subprocess.run([sys.executable,str(Path(__file__).with_name('portal-resource-sample.py')),
    '--seconds',str(a.seconds),'--label',a.round+'-'+mode],stdout=out,check=True)
  end=status();assert end['visible'] and end['mode']==mode and end['run']==begin['run']
  if mode=='motion':assert (end['draws']-begin['draws'])/(end['elapsed']-begin['elapsed'])>28, 'stimulus update rate too low'
  assert snapshot()==metadata,'service/config changed during sample'
  report=json.loads(path.read_text());report['workload']=dict(start=begin,end=end)
  report['configuration']=metadata
  report['path']=a.path
  output=subprocess.check_output(['journalctl','--user','-u','uuremote-native.service','--since',report['started_utc'],'--no-pager','-o','json'],text=True)
  windows=[]
  for line in output.splitlines():
   row=json.loads(line); msg=row.get('MESSAGE','')
   if not isinstance(msg,str) or 'Desktop latest-frame stats:' not in msg:continue
   fields={k:int(v) for k,v in re.findall(r'(window_s|window_sent|window_skipped|max_slot_age_ms|max_send_ms)=(\d+)',msg)}
   t=int(row['__MONOTONIC_TIMESTAMP'])/1e6
   if fields.get('window_s') and t-fields['window_s']>=report['measurement_start_monotonic'] and t<=report['measurement_end_monotonic']:
    windows.append(fields)
  report['whole_delivery_windows']=windows
  report['delivery_fps']=sum(w['window_sent'] for w in windows)/sum(w['window_s'] for w in windows) if windows else None
  path.write_text(json.dumps(report,indent=2)+'\n')
  print(a.round,mode,'complete',flush=True)
finally:
 control('idle')
