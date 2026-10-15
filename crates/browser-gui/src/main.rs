mod application;
mod session;

use std::{env, error::Error, path::PathBuf};

use application::{GuiApplication, GuiEvent};
use session::BrowserSession;
use winit::event_loop::{ControlFlow, EventLoop};

struct Options {
    socket: PathBuf,
    profile: String,
    tab: Option<String>,
    url: Option<String>,
}

fn options() -> Result<Options, String> {
    let mut args = env::args().skip(1);
    let mut socket = None;
    let mut profile = None;
    let mut tab = None;
    let mut url = None;

    while let Some(argument) = args.next() {
        let value = match argument.as_str() {
            "--socket" | "--profile" | "--tab" | "--url" => args
                .next()
                .ok_or_else(|| format!("{argument} requires a value"))?,
            "--help" | "-h" => {
                return Err(
                    "usage: rwb-gui --socket PATH --profile p_NAME [--tab t_ID] [--url URL]"
                        .to_owned(),
                );
            }
            _ => return Err(format!("unknown argument: {argument}")),
        };
        match argument.as_str() {
            "--socket" => socket = Some(PathBuf::from(value)),
            "--profile" => profile = Some(value),
            "--tab" => tab = Some(value),
            "--url" => url = Some(value),
            _ => unreachable!(),
        }
    }

    Ok(Options {
        socket: socket.ok_or_else(|| "--socket is required".to_owned())?,
        profile: profile.ok_or_else(|| "--profile is required".to_owned())?,
        tab,
        url,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = options().map_err(|message| {
        eprintln!("{message}");
        std::io::Error::new(std::io::ErrorKind::InvalidInput, message)
    })?;
    let session = BrowserSession::connect(
        options.socket,
        &options.profile,
        options.tab.as_deref(),
        options.url.as_deref(),
    )?;

    let event_loop = EventLoop::<GuiEvent>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut application = GuiApplication::new(session, event_loop.create_proxy());
    event_loop.run_app(&mut application)?;
    Ok(())
}
