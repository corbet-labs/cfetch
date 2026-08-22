use std::fmt;
use std::io::{self, Write as _};

fn finish(result: io::Result<()>, stream: &str) {
    match result {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(error) => panic!("failed printing to {stream}: {error}"),
    }
}

pub fn stdout(args: fmt::Arguments<'_>) {
    finish(io::stdout().lock().write_fmt(args), "stdout");
}

pub fn stdout_line(args: fmt::Arguments<'_>) {
    let mut out = io::stdout().lock();
    finish(out.write_fmt(args).and_then(|()| out.write_all(b"\n")), "stdout");
}

pub fn stderr_line(args: fmt::Arguments<'_>) {
    let mut out = io::stderr().lock();
    finish(out.write_fmt(args).and_then(|()| out.write_all(b"\n")), "stderr");
}
