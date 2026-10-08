use anyhow::Result;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::Read;

fn main() -> Result<()> {
    // 自分側の口から読む
    // 表示する
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("ls");
    cmd.cwd(std::env::current_dir()?);
    let child = pair.slave.spawn_command(cmd)?;

    let mut reader: Box<dyn Read + Send> = pair.master.try_clone_reader()?;
    let mut buf = [0u8; 1024];
    let n = reader.read(&mut buf)?;

    println!("{}バイト受け取った", n);
    println!("{}", String::from_utf8_lossy(&buf[..n]));
    Ok(())
}
