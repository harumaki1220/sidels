use anyhow::Result;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{self, Read, Write};

fn main() -> Result<()> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("zsh");
    cmd.cwd(std::env::current_dir()?);
    let child = pair.slave.spawn_command(cmd)?;

    let mut reader: Box<dyn Read + Send> = pair.master.try_clone_reader()?;
    let mut buf = [0u8; 1024];
    let mut stdout = io::stdout();

    loop {
        let n = reader.read(&mut buf)?;

        if n == 0 {
            break;
        }
        stdout.write_all(&buf[..n])?;
        stdout.flush()?;
    }
    Ok(())
}
