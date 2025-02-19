use std::{
    future::Future,
    io::BufReader,
    net::{SocketAddr, SocketAddrV4},
    sync::Arc,
};

use bevy::{tasks::IoTaskPool, utils::hashbrown::HashMap};
use tokio::sync::{
    mpsc::{self, error::TryRecvError, unbounded_channel, UnboundedReceiver, UnboundedSender},
    Mutex,
};
use tracing::{debug, error, info, warn};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{
    js_sys::{ArrayBuffer, Uint8Array},
    BinaryType, CloseEvent, ErrorEvent, MessageEvent, WebSocket,
};

use crate::transport::error::{Error, Result};
use crate::transport::{
    BoxedCloseFn, BoxedReceiver, BoxedSender, PacketReceiver, PacketSender, Transport,
    TransportBuilder, TransportEnum, LOCAL_SOCKET, MTU,
};

pub(crate) struct WebSocketClientSocketBuilder {
    pub(crate) server_addr: SocketAddr,
}

impl TransportBuilder for WebSocketClientSocketBuilder {
    fn connect(self) -> Result<TransportEnum> {
        let (serverbound_tx, serverbound_rx) = unbounded_channel::<Vec<u8>>();
        let (clientbound_tx, clientbound_rx) = unbounded_channel::<Vec<u8>>();
        let (close_tx, mut close_rx) = mpsc::channel(1);

        let sender = WebSocketClientSocketSender { serverbound_tx };

        let receiver = WebSocketClientSocketReceiver {
            buffer: [0; MTU],
            server_addr: self.server_addr,
            clientbound_rx,
        };

        info!("Starting client websocket task");

        let ws = WebSocket::new(&format!("ws://{}/", self.server_addr)).unwrap();

        ws.set_binary_type(BinaryType::Arraybuffer);

        let on_message_callback = Closure::<dyn FnMut(_)>::new(move |e: MessageEvent| {
            let msg = Uint8Array::new(&e.data()).to_vec();
            clientbound_tx
                .send(msg)
                .expect("Unable to propagate the read websocket message to the receiver");
        });

        let on_close_callback = Closure::<dyn FnMut(_)>::new(move |e: CloseEvent| {
            info!(
                "WebSocket connection closed with code {} and reason {}",
                e.code(),
                e.reason()
            );
        });

        let on_error_callback = Closure::<dyn FnMut(_)>::new(move |e: ErrorEvent| {
            error!("WebSocket connection error {}", e.message());
        });

        // need to clone these two because we move two times
        let ws_clone = ws.clone();
        let serverbound_rx = Arc::new(Mutex::new(serverbound_rx));

        let on_open_callback = Closure::<dyn FnOnce()>::once(move || {
            info!("WebSocket handshake has been successfully completed");
            let serverbound_rx = serverbound_rx.clone();
            IoTaskPool::get().spawn_local(async move {
                while let Some(msg) = serverbound_rx.lock().await.recv().await {
                    if ws_clone.ready_state() != 1 {
                        warn!("Tried to send packet through closed websocket connection");
                        break;
                    }
                    ws_clone.send_with_u8_array(&msg).unwrap();
                }
            });
        });

        let ws_clone = ws.clone();
        let listen_close_signal_callback = Closure::<dyn FnOnce()>::once(move || {
            IoTaskPool::get().spawn_local(async move {
                close_rx.recv().await;
                info!("Close websocket connection");
                ws_clone.close().unwrap();
            });
        });

        ws.set_onopen(Some(on_open_callback.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message_callback.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close_callback.as_ref().unchecked_ref()));
        ws.set_onerror(Some(on_error_callback.as_ref().unchecked_ref()));

        on_open_callback.forget();
        on_message_callback.forget();
        on_close_callback.forget();
        on_error_callback.forget();
        listen_close_signal_callback.forget();

        Ok(TransportEnum::WebSocketClient(WebSocketClientSocket {
            sender,
            receiver,
            close_sender: close_tx,
        }))
    }
}

pub struct WebSocketClientSocket {
    sender: WebSocketClientSocketSender,
    receiver: WebSocketClientSocketReceiver,
    close_sender: mpsc::Sender<()>,
}

impl Transport for WebSocketClientSocket {
    fn local_addr(&self) -> SocketAddr {
        LOCAL_SOCKET
    }

    fn split(self) -> (BoxedSender, BoxedReceiver, Option<BoxedCloseFn>) {
        let close_fn = move || {
            self.close_sender
                .blocking_send(())
                .map_err(|e| Error::from(std::io::Error::other(format!("close error: {:?}", e))))
        };
        (
            Box::new(self.sender),
            Box::new(self.receiver),
            Some(Box::new(close_fn)),
        )
    }
}

struct WebSocketClientSocketSender {
    serverbound_tx: UnboundedSender<Vec<u8>>,
}

impl PacketSender for WebSocketClientSocketSender {
    fn send(&mut self, payload: &[u8], address: &SocketAddr) -> Result<()> {
        self.serverbound_tx.send(payload.to_vec()).map_err(|e| {
            std::io::Error::other(format!("unable to send message to server: {:?}", e)).into()
        })
    }
}

struct WebSocketClientSocketReceiver {
    buffer: [u8; MTU],
    server_addr: SocketAddr,
    clientbound_rx: UnboundedReceiver<Vec<u8>>,
}

impl PacketReceiver for WebSocketClientSocketReceiver {
    fn recv(&mut self) -> Result<Option<(&mut [u8], SocketAddr)>> {
        match self.clientbound_rx.try_recv() {
            Ok(msg) => {
                self.buffer[..msg.len()].copy_from_slice(&msg);
                Ok(Some((&mut self.buffer[..msg.len()], self.server_addr)))
            }
            Err(e) => {
                if e == TryRecvError::Empty {
                    Ok(None)
                } else {
                    Err(std::io::Error::other(format!(
                        "unable to receive message from client: {}",
                        e
                    ))
                    .into())
                }
            }
        }
    }
}
