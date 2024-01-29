#![allow(unused_imports)]
#![allow(unused_variables)]
#![allow(dead_code)]

//! Run with
//! - `cargo run -- server`
//! - `cargo run -- client -c 1`
mod client;
mod protocol;
#[cfg(not(target_family = "wasm"))]
mod server;
mod shared;

use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;

use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::DefaultPlugins;
use bevy_inspector_egui::quick::WorldInspectorPlugin;
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use tracing_subscriber::fmt::format::FmtSpan;

use crate::client::ClientPluginGroup;
#[cfg(not(target_family = "wasm"))]
use crate::server::ServerPluginGroup;
use lightyear::netcode::{ClientId, Key};
use lightyear::prelude::TransportConfig;

// Use a port of 0 to automatically select a port
pub const CLIENT_PORT: u16 = 0;
pub const SERVER_PORT: u16 = 5000;
pub const PROTOCOL_ID: u64 = 0;

pub const KEY: Key = [0; 32];

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Transports {
    #[cfg(not(target_family = "wasm"))]
    Udp,
    WebTransport,
}

#[derive(Parser, PartialEq, Debug)]
enum Cli {
    SinglePlayer,
    #[cfg(not(target_family = "wasm"))]
    Server {
        #[arg(long, default_value = "false")]
        headless: bool,

        #[arg(short, long, default_value = "false")]
        inspector: bool,

        #[arg(short, long, default_value_t = SERVER_PORT)]
        port: u16,

        #[arg(short, long, value_enum, default_value_t = Transports::WebTransport)]
        transport: Transports,
    },
    Client {
        #[arg(short, long, default_value = "false")]
        inspector: bool,

        #[arg(short, long, default_value_t = 0)]
        client_id: u64,

        #[arg(long, default_value_t = CLIENT_PORT)]
        client_port: u16,

        #[arg(long, default_value_t = Ipv4Addr::LOCALHOST)]
        server_addr: Ipv4Addr,

        #[arg(short, long, default_value_t = SERVER_PORT)]
        server_port: u16,

        #[arg(short, long, value_enum, default_value_t = Transports::WebTransport)]
        transport: Transports,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let mut app = App::new();
    setup(&mut app, cli).await;

    app.run();
}

async fn setup(app: &mut App, cli: Cli) {
    match cli {
        Cli::SinglePlayer => {}
        #[cfg(not(target_family = "wasm"))]
        Cli::Server {
            headless,
            inspector,
            port,
            transport,
        } => {
            let server_plugin_group = ServerPluginGroup::new(port, transport).await;
            if !headless {
                app.add_plugins(DefaultPlugins.build().disable::<LogPlugin>());
            } else {
                app.add_plugins(MinimalPlugins);
            }
            if inspector {
                app.add_plugins(WorldInspectorPlugin::new());
            }
            app.add_plugins(server_plugin_group.build());
        }
        Cli::Client {
            inspector,
            client_id,
            client_port,
            server_addr,
            server_port,
            transport,
        } => {
            let server_addr = SocketAddr::new(server_addr.into(), server_port);
            let client_plugin_group =
                ClientPluginGroup::new(client_id, client_port, server_addr, transport);
            app.add_plugins(DefaultPlugins.build().disable::<LogPlugin>());
            if inspector {
                app.add_plugins(WorldInspectorPlugin::new());
            }
            app.add_plugins(client_plugin_group.build());
        }
    }
}
