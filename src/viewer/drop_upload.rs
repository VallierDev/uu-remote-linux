//! Files dropped on the desktop window go to the remote folder under the drop point.
//! A Windows host has no protocol message for that, so a short-lived terminal session asks the
//! remote PowerShell which window lies under the point (desktop or an Explorer folder).
//! Paths and file names never enter logs.
use super::tools::{Packet,Transport,files::{self,Entry,Files},terminal::{Event,Terminal}};
use std::{collections::HashSet,path::PathBuf,sync::{Arc,Mutex},time::{Duration,Instant}};
use tokio::sync::mpsc;

/// What the remote reported under the drop point.
#[derive(Debug,Clone,PartialEq,serde::Deserialize,serde::Serialize)]
pub struct Target {pub kind:String,pub path:String}
pub type Status=mpsc::UnboundedSender<(String,bool)>;

/// One statement per line, no here-strings: the host's PSReadLine reads the decoded text through `iex`.
/// `{NX}`/`{NY}` are the drop point relative to the monitor that holds the remote cursor (the streamed one).
const PROBE:&str=r##"$ErrorActionPreference='Stop'
if(-not ('UuDrop.D' -as [type])){Add-Type -Name D -Namespace UuDrop -MemberDefinition '[StructLayout(LayoutKind.Sequential)] public struct P{public int X;public int Y;} [StructLayout(LayoutKind.Sequential)] public struct R{public int L;public int T;public int Rt;public int B;} [StructLayout(LayoutKind.Sequential)] public struct MI{public int Size;public R Mon;public R Work;public int Flags;} [DllImport("user32.dll")] public static extern bool SetProcessDPIAware(); [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr c); [DllImport("user32.dll")] public static extern bool GetCursorPos(out P p); [DllImport("user32.dll")] public static extern IntPtr MonitorFromPoint(P p,uint f); [DllImport("user32.dll")] public static extern bool GetMonitorInfo(IntPtr m,ref MI i); [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(P p); [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h,uint f); [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h,System.Text.StringBuilder s,int n); [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h,System.Text.StringBuilder s,int n);'}
try{$a=[UuDrop.D]::SetThreadDpiAwarenessContext([IntPtr](-4))}catch{$a=[IntPtr]::Zero};if($a -eq [IntPtr]::Zero){[UuDrop.D]::SetProcessDPIAware()|Out-Null}
$c=New-Object UuDrop.D+P;[UuDrop.D]::GetCursorPos([ref]$c)|Out-Null
$i=New-Object UuDrop.D+MI;$i.Size=40;[UuDrop.D]::GetMonitorInfo([UuDrop.D]::MonitorFromPoint($c,2),[ref]$i)|Out-Null
$p=New-Object UuDrop.D+P;$p.X=[Math]::Min($i.Mon.Rt-1,[int]($i.Mon.L+{NX}*($i.Mon.Rt-$i.Mon.L)));$p.Y=[Math]::Min($i.Mon.B-1,[int]($i.Mon.T+{NY}*($i.Mon.B-$i.Mon.T)))
$h=[UuDrop.D]::GetAncestor([UuDrop.D]::WindowFromPoint($p),2)
$s=New-Object Text.StringBuilder 512;[UuDrop.D]::GetClassName($h,$s,512)|Out-Null;$cls=$s.ToString();$s.Clear()|Out-Null;[UuDrop.D]::GetWindowText($h,$s,512)|Out-Null;$title=$s.ToString()
$kind='other';$path=[Environment]::GetFolderPath('Desktop')
if($cls -eq 'Progman' -or $cls -eq 'WorkerW'){$kind='desktop'}elseif($cls -eq 'CabinetWClass' -or $cls -eq 'ExploreWClass'){$kind='virtual';try{$tabs=@((New-Object -ComObject Shell.Application).Windows()|?{[int64]$_.HWND -eq [int64]$h});$t=@(@($tabs|?{$title.StartsWith([string]$_.LocationName)})+$tabs)[0];$f=if($t){[string]$t.Document.Folder.Self.Path};if($f -match '^([A-Za-z]:\\|\\\\)'){$kind='folder';$path=$f}}catch{}}
'UU'+'DROP:'+[Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes((@{kind=$kind;path=$path}|ConvertTo-Json -Compress)))+':END'"##;
/// Wide enough that the reply never wraps in the host's ConPTY.
const COLS:u16=1000;

/// The probe as one deflated line; a long multi-line paste loses lines while the prompt redraws.
pub fn probe_line(nx:f64,ny:f64)->String{
    use base64::Engine;
    let script=PROBE.replace("{NX}",&format!("{:.5}",nx.clamp(0.,1.))).replace("{NY}",&format!("{:.5}",ny.clamp(0.,1.)));
    let mut z=flate2::write::DeflateEncoder::new(Vec::new(),flate2::Compression::best());
    std::io::Write::write_all(&mut z,script.as_bytes()).expect("in-memory deflate");
    let data=base64::engine::general_purpose::STANDARD.encode(z.finish().expect("in-memory deflate"));
    format!("iex (New-Object IO.StreamReader(New-Object IO.Compression.DeflateStream([IO.MemoryStream][Convert]::FromBase64String('{data}'),[IO.Compression.CompressionMode]::Decompress))).ReadToEnd()")
}
/// Terminal output without escape sequences and line breaks (ConPTY repaints with CSI/OSC).
fn plain(text:&str)->String{
    let mut out=String::with_capacity(text.len());let mut it=text.chars().peekable();
    while let Some(c)=it.next(){match c{
        '\x1b'=>match it.next(){
            Some('[')=>{for c in it.by_ref(){if ('@'..='~').contains(&c){break}}},
            Some(']')=>{while let Some(c)=it.next(){if c=='\x07'||(c=='\x1b'&&it.next_if_eq(&'\\').is_some()){break}}},
            _=>{},
        },
        '\r'|'\n'=>{},
        c=>out.push(c),
    }}
    out
}
fn marker(text:&str)->Option<Target>{
    use base64::Engine;
    let text=plain(text);let start=text.find("UUDROP:")?+7;let end=start+text[start..].find(":END")?;
    serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(&text[start..end]).ok()?).ok()
}
/// Runs the probe on a terminal tool connection (child process `drop-target`).
pub async fn target(io:Transport,rx:mpsc::Receiver<Packet>,nx:f64,ny:f64)->Result<Target,String>{
    let(t,mut output)=Terminal::new(io,rx);t.check_env().await?;
    let id=t.open(COLS,40).await?;
    let line=probe_line(nx,ny)+"\r";
    let result=async{
        let(start,mut last)=(Instant::now(),Instant::now());let mut text=String::new();let mut sent:Option<Instant>=None;let mut resent=false;
        loop{
            match tokio::time::timeout(Duration::from_millis(200),output.recv()).await{
                Ok(Some(Event::Output{session,data})) if session==id=>{
                    text.push_str(&String::from_utf8_lossy(&data));last=Instant::now();
                    if sent.is_some(){if let Some(found)=marker(&text){return Ok(found)}}
                },
                Ok(Some(Event::Exit{session,..}|Event::Error{session})) if session==id=>return Err("Remote shell ended".into()),
                Ok(None)=>return Err("Terminal connection closed".into()),
                _=>{},
            }
            // Type only once the prompt has settled; input that arrives while it redraws is dropped.
            match sent{
                None if (!text.is_empty()&&last.elapsed()>=Duration::from_millis(700))||start.elapsed()>=Duration::from_secs(8)=>{t.write(id,line.as_bytes()).await?;sent=Some(Instant::now());},
                Some(at) if !resent&&at.elapsed()>=Duration::from_secs(10)=>{t.write(id,line.as_bytes()).await?;resent=true;},
                _=>{},
            }
            if start.elapsed()>=Duration::from_secs(30){return Err("The remote did not report the drop location".into())}
        }
    }.await;
    let _=t.close(id).await;result
}
/// Asks a `drop-target` child; its terminal connection is separate from the desktop session.
async fn locate(device:&str,nx:f64,ny:f64)->Result<Target,String>{
    let exe=std::env::current_exe().map_err(|_|"Cannot locate the viewer executable")?;
    let child=tokio::process::Command::new(exe).args(["drop-target",device,&format!("{nx:.5}"),&format!("{ny:.5}")])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true).output();
    let out=tokio::time::timeout(Duration::from_secs(60),child).await.map_err(|_|"Drop location timed out")?.map_err(|_|"Cannot start the drop helper")?;
    String::from_utf8_lossy(&out.stdout).lines().rev().find_map(|l|serde_json::from_str(l).ok()).ok_or_else(||"The drop helper failed".into())
}

/// The official Windows host lists drives as `C:\`; this project's Linux host lists `/`.
fn windows(root:&[Entry])->bool{root.iter().any(|e|{let b=e.path.as_bytes();b.len()==3&&b[0].is_ascii_alphabetic()&&&b[1..]==b":\\"})}
/// "此电脑" lists the user folders; Desktop keeps its on-disk name on Windows, `桌面` with a Chinese locale.
fn desktop(root:&[Entry])->Option<String>{
    root.iter().find(|e|["desktop","桌面"].iter().any(|d|files::base_name(&e.path).eq_ignore_ascii_case(d)||e.name.eq_ignore_ascii_case(d))).map(|e|e.path.clone())
}
/// First free `name`, `name (1)`, ... among `taken`; the host would refuse or overwrite an existing name.
fn free_name(taken:&HashSet<String>,name:&str,file:bool,fold:bool)->String{
    let key=|n:&str|if fold{n.to_lowercase()}else{n.to_owned()};
    let(stem,ext)=match name.rfind('.'){Some(i) if file&&i>0=>(&name[..i],&name[i..]),_=>(name,"")};
    (0..).map(|n|if n==0{name.to_owned()}else{format!("{stem} ({n}){ext}")}).find(|c|!taken.contains(&key(c))).expect("unbounded")
}
enum Step {Mkdir(String),Put(PathBuf,String)}
const MAX_STEPS:usize=10_000;
/// Dropped files and folders (recursively; links are skipped) as remote operations under `dir`.
fn plan(paths:&[PathBuf],dir:&str,taken:&mut HashSet<String>,fold:bool)->Result<Vec<Step>,String>{
    let mut steps=Vec::new();
    for path in paths{
        let meta=std::fs::symlink_metadata(path).map_err(|_|"Cannot read a dropped file")?;
        let Some(name)=path.file_name().map(|n|n.to_string_lossy().into_owned()) else{continue};
        let name=free_name(taken,&name,meta.is_file(),fold);taken.insert(if fold{name.to_lowercase()}else{name.clone()});
        let remote=files::remote_join(dir,&name);
        if meta.is_file(){steps.push(Step::Put(path.clone(),remote));continue}
        if !meta.is_dir(){continue}
        let mut stack=vec![(path.clone(),remote)];
        while let Some((local,remote))=stack.pop(){
            steps.push(Step::Mkdir(remote.clone()));
            let mut entries:Vec<_>=std::fs::read_dir(&local).map_err(|_|"Cannot read a dropped folder")?.flatten().collect();
            entries.sort_by_key(|e|e.file_name());
            for e in entries{
                let Ok(kind)=e.file_type() else{continue};let child=files::remote_join(&remote,&e.file_name().to_string_lossy());
                if kind.is_file(){steps.push(Step::Put(e.path(),child));}else if kind.is_dir(){stack.push((e.path(),child));}
            }
            if steps.len()>MAX_STEPS{return Err("Too many files in the dropped folder".into())}
        }
    }
    Ok(steps)
}

/// Upload over the desktop session's FILE channel to the folder under the drop point.
/// Returns the message for the finished transfer.
pub async fn run(io:Transport,rx:mpsc::Receiver<Packet>,device:&str,mac:bool,paths:Vec<PathBuf>,nx:f64,ny:f64,status:&Status)->Result<String,String>{
    let(files,mut progress)=Files::new(io,rx);
    let _=status.send((crate::i18n::s("正在确定放置位置…","Finding where you dropped it…").into(),false));
    let(dir,fallback)=if mac{("~/Desktop".to_owned(),false)}else{
        let root=files.list_dir(":/").await?;
        let desk=desktop(&root);
        if windows(&root){
            match locate(device,nx,ny).await{
                Ok(t)=>{log::info!("Viewer drop target: kind={}",t.kind);let exact=matches!(t.kind.as_str(),"desktop"|"folder");(t.path,!exact)},
                Err(e)=>{log::warn!("Viewer drop target unknown: {e}");(desk.ok_or("Remote desktop folder not found")?,true)},
            }
        }else{(desk.ok_or("Remote desktop folder not found")?,false)}
    };
    let fold=mac||dir.contains('\\');
    let mut taken:HashSet<String>=files.list_dir(&dir).await?.iter().map(|e|if fold{e.name.to_lowercase()}else{e.name.clone()}).collect();
    let steps=plan(&paths,&dir,&mut taken,fold)?;
    let total=steps.iter().filter(|s|matches!(s,Step::Put(..))).count();
    let label=Arc::new(Mutex::new(String::new()));
    let(shown,tx)=(label.clone(),status.clone());
    let reporter=tokio::spawn(async move{
        let mut last=Instant::now()-Duration::from_secs(1);
        while let Some(p)=progress.recv().await{
            if last.elapsed()<Duration::from_millis(250)&&p.transferred<p.total{continue}last=Instant::now();
            let percent=if p.total>0{p.transferred*100/p.total}else{100};
            let _=tx.send((format!("{} {percent}%",shown.lock().unwrap()),false));
        }
    });
    let _reporter=super::tools::Cancel(reporter.abort_handle());
    let mut done=0;
    for step in steps{match step{
        Step::Mkdir(path)=>files.mkdir(&path).await?,
        Step::Put(local,remote)=>{
            done+=1;let name=local.file_name().map(|n|n.to_string_lossy().into_owned()).unwrap_or_default();
            *label.lock().unwrap()=if total>1{crate::tr!("正在上传 {name}（{done}/{total}）","Uploading {name} ({done}/{total})")}else{crate::tr!("正在上传 {name}","Uploading {name}")};
            let _=status.send((format!("{} 0%",label.lock().unwrap()),false));
            files.upload(&local,&remote).await?;
        },
    }}
    Ok(if fallback{crate::tr!("已保存到 {dir}（放置处不是文件夹）","Saved to {dir} (not dropped on a folder)")}else{crate::tr!("已保存到 {dir}","Saved to {dir}")})
}

#[cfg(test)] mod tests {
    use super::*;
    fn entry(kind:u64,name:&str,path:&str)->Entry{Entry{kind,name:name.into(),size:0,modified:0,path:path.into()}}
    #[test] fn probe_is_one_line_and_inflates_to_the_script_with_the_point(){
        use base64::Engine;
        let line=probe_line(0.25,1.5);assert!(!line.contains(['\n','\r']));assert!(line.len()<2000,"{}",line.len());
        let data=line.split('\'').nth(1).unwrap();
        let raw=base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        let mut script=String::new();std::io::Read::read_to_string(&mut flate2::read::DeflateDecoder::new(raw.as_slice()),&mut script).unwrap();
        assert!(script.contains("$i.Mon.L+0.25000*") && script.contains("$i.Mon.T+1.00000*"));
        assert!(!script.contains("{NX}") && !script.contains("@'"));
        assert!(!script.contains("UUDROP"),"the echoed command must not contain the marker");
    }
    #[test] fn marker_survives_conpty_repaints(){
        use base64::Engine;
        let json=r#"{"kind":"folder","path":"C:\\Users\\k\\Downloads"}"#;
        let b=base64::engine::general_purpose::STANDARD.encode(json);
        let (a,z)=b.split_at(10);
        let noisy=format!("\x1b]0;powershell.exe\x07\x1b[?25lPS C:\\> iex (...)\r\n\x1b[93mUU\x1b[mDROP:{a}\x1b[?25h\r\n{z}:END\x1b[K\r\nPS C:\\>");
        assert_eq!(marker(&noisy),Some(Target{kind:"folder".into(),path:"C:\\Users\\k\\Downloads".into()}));
        assert_eq!(marker("UUDROP:not base64!:END"),None);assert_eq!(marker("PS C:\\> "),None);
    }
    #[test] fn root_listing_tells_windows_and_the_desktop(){
        let win=[entry(3," (C:)","C:\\"),entry(0,"Documents","C:\\Users\\k\\Documents"),entry(0,"Desktop","C:\\Users\\k\\Desktop")];
        assert!(windows(&win));assert_eq!(desktop(&win).as_deref(),Some("C:\\Users\\k\\Desktop"));
        let linux=[entry(0,"文档","/home/k/文档"),entry(0,"桌面","/home/k/桌面"),entry(0,"k","/home/k"),entry(4,"/","/")];
        assert!(!windows(&linux));assert_eq!(desktop(&linux).as_deref(),Some("/home/k/桌面"));
        assert_eq!(desktop(&[entry(0,"k","/home/k")]),None);
    }
    #[test] fn names_never_replace_remote_files(){
        let taken:HashSet<String>=["a.txt","a (1).txt","dir"].map(String::from).into();
        assert_eq!(free_name(&taken,"A.TXT",true,true),"A (2).TXT");assert_eq!(free_name(&taken,"A.TXT",true,false),"A.TXT");
        assert_eq!(free_name(&taken,"dir",false,true),"dir (1)");assert_eq!(free_name(&taken,".env",true,true),".env");
        assert_eq!(free_name(&taken,"x.tar.gz",true,true),"x.tar.gz");
    }
    #[test] fn folders_become_mkdir_then_files_and_links_are_skipped(){
        let root=std::env::temp_dir().join(format!("uu-drop-{}",std::process::id()));let _=std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("pics/sub")).unwrap();std::fs::write(root.join("pics/a.png"),b"a").unwrap();
        std::fs::write(root.join("pics/sub/b.png"),b"b").unwrap();std::fs::write(root.join("note.txt"),b"n").unwrap();
        std::os::unix::fs::symlink("/etc/passwd",root.join("pics/link")).unwrap();
        let mut taken:HashSet<String>=["note.txt".into()].into();
        let steps=plan(&[root.join("note.txt"),root.join("pics")],"C:\\Users\\k\\Desktop",&mut taken,true).unwrap();
        let shown:Vec<String>=steps.iter().map(|s|match s{Step::Mkdir(r)=>format!("mkdir {r}"),Step::Put(l,r)=>format!("put {} {r}",l.strip_prefix(&root).unwrap().display())}).collect();
        assert_eq!(shown,["put note.txt C:\\Users\\k\\Desktop\\note (1).txt","mkdir C:\\Users\\k\\Desktop\\pics","put pics/a.png C:\\Users\\k\\Desktop\\pics\\a.png",
            "mkdir C:\\Users\\k\\Desktop\\pics\\sub","put pics/sub/b.png C:\\Users\\k\\Desktop\\pics\\sub\\b.png"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
