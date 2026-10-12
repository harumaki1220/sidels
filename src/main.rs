use anyhow::Result;
use crossterm::terminal;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{self, Read, Write};

fn main() -> Result<()> {
    let pty_system = native_pty_system();
    let (cols, rows) = terminal::size()?;
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cmd = CommandBuilder::new("zsh");
    cmd.cwd(std::env::current_dir()?);
    let child = pair.slave.spawn_command(cmd)?;
    drop(pair.slave);

    // キーボード → zsh

    let mut writer = pair.master.take_writer()?;
    terminal::enable_raw_mode()?;

    std::thread::spawn(move || {
        let mut stdin = io::stdin();
        let mut buf = [0u8; 1024];
        loop {
            let n = stdin.read(&mut buf).unwrap();

            if n == 0 {
                break;
            }

            writer.write_all(&buf[..n]).unwrap();
            writer.flush().unwrap();
        }
    });

    // zshの出力 → 画面

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

    terminal::disable_raw_mode()?;
    Ok(())
}
