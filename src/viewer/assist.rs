//! Assistance secrets travel only through memory and the child's stdin pipe.
use std::{io::BufRead,path::Path,process::Stdio};
use tokio::{io::AsyncWriteExt,process::{Child,Command}};

pub fn read_code(input: impl BufRead) -> Result<String,String> {
    let mut code=String::new();
    input.take(1025).read_line(&mut code).map_err(|_|crate::i18n::s("无法从 stdin 读取验证码","Cannot read the code from stdin"))?;
    if code.len()>1024 {return Err(crate::i18n::s("验证码过长","Code is too long").into())}
    if code.ends_with('\n') {code.pop();if code.ends_with('\r'){code.pop();}}
    if code.trim().is_empty() || code.chars().any(char::is_control) {return Err(crate::i18n::s("请输入有效验证码","Enter a valid code").into())}
    Ok(code)
}
pub fn command(exe:&Path,id:&str)->Command {
    let mut command=Command::new(exe);
    command.args(["assist",id]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
    command
}
pub async fn send_code(child:&mut Child,code:&str)->Result<(),String> {
    let result=async {
        let mut stdin=child.stdin.take().ok_or(crate::i18n::s("无法打开会话输入管道","Cannot open the session input pipe"))?;
        stdin.write_all(code.as_bytes()).await.map_err(|_|crate::i18n::s("无法向会话传递验证码","Cannot pass the code to the session"))?;
        stdin.write_all(b"\n").await.map_err(|_|crate::i18n::s("无法向会话传递验证码","Cannot pass the code to the session"))?;
        stdin.shutdown().await.map_err(|_|crate::i18n::s("无法关闭会话输入管道","Cannot close the session input pipe"))
    }.await;
    if result.is_err() {let _=child.kill().await;}
    result.map_err(str::to_owned)
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn stdin_reads_one_line_without_changing_code() {
        assert_eq!(read_code(&b"a b\r\nignored\n"[..]).unwrap(),"a b");
        assert_eq!(read_code(&b"fixture"[..]).unwrap(),"fixture");
        for input in ["","\n"," \r\n","a\tb\n","a\0b\n",&"x".repeat(1025)] {assert!(read_code(input.as_bytes()).is_err());}
    }
    #[tokio::test] async fn launcher_passes_only_id_in_arguments_and_closes_stdin() {
        let command=command(Path::new("/bin/sh"),"synthetic-id");
        assert_eq!(command.as_std().get_args().collect::<Vec<_>>(),["assist","synthetic-id"]);
        // Local process mock: echo the pipe after EOF, without files or credentials.
        let mut child=Command::new("/bin/sh").args(["-c","cat"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        send_code(&mut child,"synthetic-code").await.unwrap();
        let output=child.wait_with_output().await.unwrap();
        assert!(output.status.success());assert_eq!(output.stdout,b"synthetic-code\n");
    }
}
