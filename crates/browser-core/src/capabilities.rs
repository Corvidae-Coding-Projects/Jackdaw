use std::collections::BTreeMap;

use browser_protocol::{
    CapabilityManifest, Command, CommandCapability, EffectState, NavigationCommand,
    PROTOCOL_VERSION, ProfileCommand, TabCommand,
};

pub(crate) fn capabilities() -> CapabilityManifest {
    CapabilityManifest {
        protocol_version: PROTOCOL_VERSION,
        product_version: env!("CARGO_PKG_VERSION").to_owned(),
        engine_versions: BTreeMap::from([
            ("boa".to_owned(), "0.22.0".to_owned()),
            ("blitz".to_owned(), "0.3.0-beta.2".to_owned()),
            ("rustls".to_owned(), "0.23.35".to_owned()),
            ("oxitls-rustcrypto-provider".to_owned(), "0.3.0".to_owned()),
            ("oxitls-webpki-roots".to_owned(), "0.3.0".to_owned()),
        ]),
        commands: [
            "status",
            "daemon.stop",
            "version",
            "capabilities",
            "profile.create",
            "profile.list",
            "profile.open",
            "profile.close",
            "profile.remove",
            "tab.new",
            "tab.list",
            "tab.close",
            "tab.activate",
            "navigate.open",
            "navigate.back",
            "navigate.forward",
            "navigate.reload",
            "navigate.stop",
            "observe.text",
            "observe.controls",
            "observe.links",
            "observe.all",
            "read",
            "find",
            "screenshot",
            "frame_input",
            "interact.click",
            "interact.focus",
            "interact.fill",
            "interact.type",
            "interact.key",
            "interact.select",
            "interact.check",
            "interact.submit",
            "interact.scroll",
            "wait.idle",
            "wait.url",
            "wait.text",
            "wait.role_name",
            "wait.target_state",
            "wait.download",
            "upload",
            "download.start",
            "download.list",
            "download.wait",
            "download.cancel",
            "operation.get",
            "events.read",
            "diagnostic.render_frame",
            "diagnostic.evaluate",
            "diagnostic.console",
            "diagnostic.network",
        ]
        .into_iter()
        .map(|name| CommandCapability {
            name: name.to_owned(),
            schema_version: 1,
        })
        .collect(),
        observation_modes: vec![
            "text".to_owned(),
            "controls".to_owned(),
            "links".to_owned(),
            "all".to_owned(),
        ],
        frame_formats: vec!["rgba8_premultiplied".to_owned()],
        limitations: Vec::new(),
    }
}

pub(crate) fn completed_effect(command: &Command) -> EffectState {
    match command {
        Command::Status
        | Command::Version
        | Command::Capabilities
        | Command::Navigate(NavigationCommand::Stop)
        | Command::Profile(ProfileCommand::List)
        | Command::Tab(TabCommand::List)
        | Command::Download(browser_protocol::DownloadCommand::List)
        | Command::Observe(_)
        | Command::Read(_)
        | Command::Find(_)
        | Command::Screenshot(_)
        | Command::Wait(_)
        | Command::Operation(_)
        | Command::Events(_)
        | Command::Diagnostic(browser_protocol::DiagnosticCommand::RenderFrame)
        | Command::Diagnostic(browser_protocol::DiagnosticCommand::Console)
        | Command::Diagnostic(browser_protocol::DiagnosticCommand::Network) => EffectState::None,
        _ => EffectState::Applied,
    }
}
