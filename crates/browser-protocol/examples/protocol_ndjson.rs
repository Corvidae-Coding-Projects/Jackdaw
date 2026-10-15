use std::io::{self, BufRead, Write};

use browser_protocol::CommandRequest;
use serde::Serialize;

#[derive(Serialize)]
#[serde(untagged)]
enum Output {
    Request(CommandRequest),
    Error { error: String },
}

fn decode(line: &str) -> Output {
    match serde_json::from_str::<CommandRequest>(line) {
        Ok(request) => Output::Request(request),
        Err(error) => Output::Error {
            error: error.to_string(),
        },
    }
}

fn write_output(stdout: &mut impl Write, line: &str) -> Result<(), Box<dyn std::error::Error>> {
    serde_json::to_writer(&mut *stdout, &decode(line))?;
    stdout.write_all(b"\n")?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();

    if arguments.is_empty() {
        for line in io::stdin().lock().lines() {
            write_output(&mut stdout, &line?)?;
        }
    } else {
        for line in arguments {
            write_output(&mut stdout, &line)?;
        }
    }
    stdout.flush()?;
    Ok(())
}
