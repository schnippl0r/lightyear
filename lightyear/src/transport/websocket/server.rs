use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use async_compat::Compat;
use bevy::tasks::{futures_lite, IoTaskPool};
use bevy::utils::hashbrown::HashMap;

use tracing::{info, trace};
use tracing_log::log::error;

use futures_util::{
    future, pin_mut,
    stream::{SplitSink, TryStreamExt},
    SinkExt, StreamExt, TryFutureExt,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc::{error::TryRecvError, unbounded_channel, UnboundedReceiver, UnboundedSender},
};
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};

use crate::transport::error::{Error, Result};
use crate::transport::{PacketReceiver, PacketSender, Transport};

use super::MTU;

pub struct WebSocketServerSocket {
    server_addr: SocketAddr,
    sender: Option<WebSocketServerSocketSender>,
    receiver: Option<WebSocketServerSocketReceiver>,
}

impl WebSocketServerSocket {
    pub(crate) fn new(server_addr: SocketAddr) -> Self {
        Self {
            server_addr,
            sender: None,
            receiver: None,
        }
    }

    /*fn get_tls_acceptor(&self) -> Option<TlsAcceptor> {
        if let Some(config) = &self.tls_config {
            let server_config = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    certs(&mut BufReader::new(&*config.certs))
                        .map(|e| e.unwrap())
                        .collect(),
                    rsa_private_keys(&mut BufReader::new(&*config.keys))
                        .map(|e| e.unwrap().into())
                        .next()
                        .unwrap(),
                )
                .unwrap();
            Some(TlsAcceptor::from(Arc::new(server_config)))
        } else {
            None
        }
    }*/
}

type ClientBoundTxMap = Arc<Mutex<HashMap<SocketAddr, UnboundedSender<Message>>>>;

impl Transport for WebSocketServerSocket {
    fn local_addr(&self) -> SocketAddr {
        self.server_addr
    }

    fn connect(&mut self) -> Result<()> {
        let (serverbound_tx, serverbound_rx) = unbounded_channel::<(SocketAddr, Message)>();
        let clientbound_tx_map = ClientBoundTxMap::new(Mutex::new(HashMap::new()));

        self.sender = Some(WebSocketServerSocketSender {
            server_addr: self.server_addr,
            addr_to_clientbound_tx: clientbound_tx_map.clone(),
        });

        self.receiver = Some(WebSocketServerSocketReceiver {
            buffer: [0; MTU],
            server_addr: self.server_addr,
            serverbound_rx,
        });

        let listener = IoTaskPool::get()
            .scope(|scope| {
                scope.spawn(async move {
                    info!("Starting server websocket task");
                    TcpListener::bind(self.server_addr).await
                })
            })
            .pop()
            .unwrap()?;

        IoTaskPool::get()
            .spawn(Compat::new(async move {
                info!("Starting server websocket task");
                while let Ok((stream, addr)) = listener.accept().await {
                    let clientbound_tx_map = clientbound_tx_map.clone();
                    let serverbound_tx = serverbound_tx.clone();

                    let ws_stream = tokio_tungstenite::accept_async(stream)
                        .await
                        .expect("Error during the websocket handshake occurred");
                    info!("New WebSocket connection: {}", addr);

                    let (clientbound_tx, mut clientbound_rx) = unbounded_channel::<Message>();
                    let (mut write, mut read) = ws_stream.split();

                    clientbound_tx_map
                        .lock()
                        .unwrap()
                        .insert(addr, clientbound_tx);

                    let serverbound_tx = serverbound_tx.clone();

                    let clientbound_handle = IoTaskPool::get().spawn(async move {
                        while let Some(msg) = clientbound_rx.recv().await {
                            write
                                .send(msg)
                                .await
                                .map_err(|e| {
                                    error!("Encountered error while sending websocket msg: {}", e);
                                })
                                .unwrap();
                        }
                        write.close().await.unwrap_or_else(|e| {
                            error!("Error closing websocket: {:?}", e);
                        });
                    });
                    let serverbound_handle = IoTaskPool::get().spawn(async move {
                        while let Some(msg) = read.next().await {
                            match msg {
                                Ok(msg) => {
                                    serverbound_tx.send((addr, msg)).unwrap_or_else(|e| {
                                        error!("receive websocket error: {:?}", e)
                                    });
                                }
                                Err(e) => {
                                    error!("receive websocket error: {:?}", e);
                                }
                            }
                        }
                    });

                    let _closed =
                        futures_lite::future::or(clientbound_handle, serverbound_handle).await;

                    info!("Connection with {} closed", addr);
                    clientbound_tx_map.lock().unwrap().remove(&addr);
                    // dropping the task handles cancels them
                }
            }))
            .detach();
        Ok(())
    }

    fn split(&mut self) -> (&mut (dyn PacketSender + '_), &mut (dyn PacketReceiver + '_)) {
        (
            self.sender.as_mut().unwrap(),
            self.receiver.as_mut().unwrap(),
        )
    }

    // fn split(&mut self) -> (&mut Box<dyn PacketSender>, &mut Box<dyn PacketReceiver>) {
    //     (
    //         &mut Box::new(self.sender.as_mut()),
    //         &mut Box::new(self.receiver.as_mut()),
    //     )
    // }
}

struct WebSocketServerSocketSender {
    server_addr: SocketAddr,
    addr_to_clientbound_tx: ClientBoundTxMap,
}

impl PacketSender for WebSocketServerSocketSender {
    fn send(&mut self, payload: &[u8], address: &SocketAddr) -> Result<()> {
        if let Some(clientbound_tx) = self.addr_to_clientbound_tx.lock().unwrap().get(address) {
            clientbound_tx
                .send(Message::Binary(payload.to_vec()))
                .map_err(|e| {
                    Error::WebSocket(
                        std::io::Error::other(format!("unable to send message to client: {}", e))
                            .into(),
                    )
                })
        } else {
            // consider that if the channel doesn't exist, it's because the connection was closed
            Ok(())
            // Err(std::io::Error::other(format!(
            //     "unable to find channel for client: {}",
            //     address
            // )))
        }
    }
}

struct WebSocketServerSocketReceiver {
    buffer: [u8; MTU],
    server_addr: SocketAddr,
    serverbound_rx: UnboundedReceiver<(SocketAddr, Message)>,
}

impl PacketReceiver for WebSocketServerSocketReceiver {
    fn recv(&mut self) -> Result<Option<(&mut [u8], SocketAddr)>> {
        match self.serverbound_rx.try_recv() {
            Ok((addr, msg)) => match msg {
                Message::Binary(buf) => {
                    self.buffer[..buf.len()].copy_from_slice(&buf);
                    Ok(Some((&mut self.buffer[..buf.len()], addr)))
                }
                Message::Close(frame) => {
                    info!("WebSocket connection closed (Frame: {:?})", frame);
                    Ok(None)
                }
                _ => Ok(None),
            },
            Err(e) => {
                if e == TryRecvError::Empty {
                    Ok(None)
                } else {
                    Err(Error::WebSocket(
                        std::io::Error::other(format!(
                            "unable to receive message from client: {}",
                            e
                        ))
                        .into(),
                    ))
                }
            }
        }
    }
}
