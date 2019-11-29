use super::misc::StreamID;
use super::spi::{Rx, Transport, Tx};
use crate::errors::{ErrorKind, RSocketError};
use crate::frame::{self, Body, Frame};
use crate::payload::{Payload, SetupPayload};
use crate::result::RSocketResult;
use crate::spi::{Acceptor, EmptyRSocket, Flux, Mono, RSocket};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::{future, Sink, SinkExt, Stream, StreamExt};
use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use tokio::net::TcpListener;
use tokio::prelude::*;
use tokio::sync::oneshot::{Receiver, Sender};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub(crate) struct DuplexSocket {
    seq: StreamID,
    responder: Responder,
    tx: Tx,
    handlers: Arc<Handlers>,
}

#[derive(Clone)]
struct Responder {
    inner: Arc<RwLock<Box<dyn RSocket>>>,
}

#[derive(Debug)]
enum Handler {
    Request(oneshot::Sender<RSocketResult<Payload>>),
    Stream(mpsc::UnboundedSender<RSocketResult<Payload>>),
}

#[derive(Debug)]
struct Handlers {
    map: RwLock<HashMap<u32, Handler>>,
}

impl DuplexSocket {
    pub(crate) fn new(first_stream_id: u32, tx: Tx) -> DuplexSocket {
        DuplexSocket {
            seq: StreamID::from(first_stream_id),
            tx,
            responder: Responder::new(),
            handlers: Arc::new(Handlers::new()),
        }
    }

    pub(crate) fn close(self) {
        drop(self.tx);
    }

    pub(crate) async fn setup(&self, setup: SetupPayload) {
        let mut bu = frame::Setup::builder(0, 0);
        if let Some(s) = setup.data_mime_type() {
            bu = bu.set_mime_data(&s);
        }
        if let Some(s) = setup.metadata_mime_type() {
            bu = bu.set_mime_metadata(&s);
        }
        bu = bu.set_keepalive(setup.keepalive_interval());
        bu = bu.set_lifetime(setup.keepalive_lifetime());
        let (d, m) = setup.split();
        if let Some(b) = d {
            bu = bu.set_data(b);
        }
        if let Some(b) = m {
            bu = bu.set_metadata(b);
        }
        self.send_frame(bu.build()).await;
    }

    async fn send_frame(&self, sending: Frame) {
        let tx = self.tx.clone();
        tx.send(sending).unwrap();
    }

    fn register_handler(&self, sid: u32, handler: Handler) {
        let handlers: Arc<Handlers> = self.handlers.clone();
        let mut senders = handlers.map.write().unwrap();
        senders.insert(sid, handler);
    }

    pub(crate) async fn event_loop(&self, acceptor: Acceptor, mut rx: Rx) {
        while let Some(msg) = rx.recv().await {
            let sid = msg.get_stream_id();
            let flag = msg.get_flag();
            debug!("<--- RCV: {:?}", msg);
            match msg.get_body() {
                Body::Setup(v) => self.on_setup(&acceptor, sid, flag, SetupPayload::from(v)),
                Body::Resume(v) => {
                    // TODO: support resume
                }
                Body::ResumeOK(v) => {
                    // TODO: support resume ok
                }
                Body::MetadataPush(v) => {
                    let input = Payload::from(v);
                    self.on_metadata_push(input).await;
                }
                Body::RequestFNF(v) => {
                    let input = Payload::from(v);
                    self.on_fire_and_forget(sid, flag, input).await;
                }
                Body::RequestResponse(v) => {
                    let input = Payload::from(v);
                    self.on_request_response(sid, flag, input).await;
                }
                Body::RequestStream(v) => {
                    let input = Payload::from(v);
                    self.on_request_stream(sid, flag, input).await;
                }
                Body::RequestChannel(v) => {
                    // TODO: support channel
                }
                Body::Payload(v) => {
                    let input = Payload::from(v);
                    self.on_payload(sid, flag, input).await;
                }
                Body::Keepalive(v) => {
                    if flag & frame::FLAG_RESPOND != 0 {
                        debug!("got keepalive: {:?}", v);
                        self.on_keepalive(v).await;
                    }
                }
                Body::RequestN(v) => {
                    // TODO: support RequestN
                }
                Body::Error(v) => {
                    // TODO: support error
                }
                Body::Cancel() => {
                    // TODO: support cancel
                }
                Body::Lease(v) => {
                    // TODO: support Lease
                }
            }
        }
    }

    #[inline]
    async fn on_payload(&self, sid: u32, flag: u16, input: Payload) {
        // pick handler
        let handlers = self.handlers.clone();
        let mut senders = handlers.map.write().unwrap();
        // fire event!
        match senders.remove(&sid).unwrap() {
            Handler::Request(sender) => sender.send(Ok(input)).unwrap(),
            Handler::Stream(sender) => {
                if flag & frame::FLAG_NEXT != 0 {
                    sender.clone().send(Ok(input)).unwrap();
                }
                if flag & frame::FLAG_COMPLETE != 0 {
                    // steam end
                    drop(sender);
                } else {
                    senders.insert(sid, Handler::Stream(sender));
                }
            }
        };
    }

    #[inline]
    fn on_setup(&self, acceptor: &Acceptor, sid: u32, flag: u16, setup: SetupPayload) {
        match acceptor {
            Acceptor::Generate(gen) => {
                self.responder.set(gen(setup, Box::new(self.clone())));
            }
            Acceptor::Empty() => {
                self.responder.set(Box::new(EmptyRSocket));
            }
            // TODO: How to direct???
            // Acceptor::Direct(sk) => self.responder.set(sk),
            _ => (),
        }
    }

    #[inline]
    async fn on_fire_and_forget(&self, sid: u32, flag: u16, input: Payload) {
        match self.responder.clone().fire_and_forget(input).await {
            Ok(()) => (),
            Err(e) => error!("respoond fnf failed: {}", e),
        }
    }

    #[inline]
    async fn on_request_response(&self, sid: u32, _flag: u16, input: Payload) {
        let responder = self.responder.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let sending = match responder.request_response(input).await {
                Ok(it) => {
                    let (d, m) = it.split();
                    let mut bu = frame::Payload::builder(sid, frame::FLAG_COMPLETE);
                    if let Some(b) = d {
                        bu = bu.set_data(b);
                    }
                    if let Some(b) = m {
                        bu = bu.set_metadata(b);
                    }
                    bu.build()
                }
                Err(e) => frame::Error::builder(sid, 0)
                    .set_code(frame::ERR_APPLICATION)
                    .set_data(Bytes::from("TODO: should be error details"))
                    .build(),
            };
            tx.send(sending).unwrap();
        });
    }

    #[inline]
    async fn on_request_stream(&self, sid: u32, flag: u16, input: Payload) {
        let responder = self.responder.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let mut payloads = responder.request_stream(input);
            while let Some(it) = payloads.next().await {
                // TODO: process error.
                let (d, m) = it.unwrap().split();
                let mut bu = frame::Payload::builder(sid, frame::FLAG_NEXT);
                if let Some(b) = d {
                    bu = bu.set_data(b);
                }
                if let Some(b) = m {
                    bu = bu.set_metadata(b);
                }
                let sending = bu.build();
                tx.send(sending).unwrap();
            }
            let complete = frame::Payload::builder(sid, frame::FLAG_COMPLETE).build();
            tx.send(complete).unwrap();
        });
    }

    #[inline]
    async fn on_metadata_push(&self, input: Payload) {
        if let Err(e) = self.responder.clone().metadata_push(input).await {
            error!("metadata_push failed: {}", e);
        }
    }

    #[inline]
    async fn on_keepalive(&self, keepalive: frame::Keepalive) {
        let tx = self.tx.clone();
        let (data, _) = keepalive.split();
        let mut sending = frame::Keepalive::builder(0, 0);
        if let Some(b) = data {
            sending = sending.set_data(b);
        }
        tx.send(sending.build()).unwrap();
    }
}

impl RSocket for DuplexSocket {
    fn metadata_push(&self, req: Payload) -> Mono<()> {
        let sid = self.seq.next();
        let tx = self.tx.clone();
        Box::pin(async move {
            let (_d, m) = req.split();
            let mut bu = frame::MetadataPush::builder(sid, 0);
            if let Some(b) = m {
                bu = bu.set_metadata(b);
            }
            let sending = bu.build();
            match tx.send(sending) {
                Ok(()) => Ok(()),
                Err(_e) => Err(RSocketError::from(ErrorKind::WithDescription(
                    "send metadata_push failed",
                ))),
            }
        })
    }
    fn fire_and_forget(&self, req: Payload) -> Mono<()> {
        let sid = self.seq.next();
        let tx = self.tx.clone();

        Box::pin(async move {
            let (d, m) = req.split();
            let mut bu = frame::RequestFNF::builder(sid, 0);
            if let Some(b) = d {
                bu = bu.set_data(b);
            }
            if let Some(b) = m {
                bu = bu.set_metadata(b);
            }
            let sending = bu.build();
            match tx.send(sending) {
                Ok(()) => Ok(()),
                Err(_e) => Err(RSocketError::from(ErrorKind::WithDescription(
                    "send fire_and_forget failed",
                ))),
            }
        })
    }
    fn request_response(&self, req: Payload) -> Mono<Payload> {
        let (tx, rx) = oneshot::channel::<RSocketResult<Payload>>();
        let sid = self.seq.next();
        // register handler
        self.register_handler(sid, Handler::Request(tx));

        let sender = self.tx.clone();
        tokio::spawn(async move {
            let (d, m) = req.split();
            // crate request frame
            let mut bu = frame::RequestResponse::builder(sid, 0);
            if let Some(b) = d {
                bu = bu.set_data(b);
            }
            if let Some(b) = m {
                bu = bu.set_metadata(b);
            }
            let sending = bu.build();
            // send frame
            sender.send(sending).unwrap();
        });
        Box::pin(async move {
            match rx.await {
                Ok(v) => v,
                Err(_e) => Err(RSocketError::from(ErrorKind::WithDescription(
                    "request_response failed",
                ))),
            }
        })
    }

    fn request_stream(&self, input: Payload) -> Flux<Payload> {
        let sid = self.seq.next();
        let tx = self.tx.clone();
        // register handler
        let (sender, receiver) = mpsc::unbounded_channel::<RSocketResult<Payload>>();
        self.register_handler(sid, Handler::Stream(sender));
        tokio::spawn(async move {
            let (d, m) = input.split();
            // crate stream frame
            let mut bu = frame::RequestStream::builder(sid, 0);
            if let Some(b) = d {
                bu = bu.set_data(b);
            }
            if let Some(b) = m {
                bu = bu.set_metadata(b);
            }
            let sending = bu.build();
            tx.send(sending).unwrap();
        });
        Box::pin(receiver)
    }
}

impl Handlers {
    fn new() -> Handlers {
        Handlers {
            map: RwLock::new(HashMap::new()),
        }
    }
}

impl From<Box<dyn RSocket>> for Responder {
    fn from(input: Box<dyn RSocket>) -> Responder {
        Responder {
            inner: Arc::new(RwLock::new(input)),
        }
    }
}

impl Responder {
    fn new() -> Responder {
        let bx = Box::new(EmptyRSocket);
        Responder {
            inner: Arc::new(RwLock::new(bx)),
        }
    }

    fn set(&self, rs: Box<dyn RSocket>) {
        let inner = self.inner.clone();
        let mut v = inner.write().unwrap();
        *v = rs;
    }
}

impl RSocket for Responder {
    fn metadata_push(&self, req: Payload) -> Mono<()> {
        let inner = self.inner.read().unwrap();
        (*inner).metadata_push(req)
    }

    fn fire_and_forget(&self, req: Payload) -> Mono<()> {
        let inner = self.inner.read().unwrap();
        (*inner).fire_and_forget(req)
    }

    fn request_response(&self, req: Payload) -> Mono<Payload> {
        let inner = self.inner.read().unwrap();
        (*inner).request_response(req)
    }

    fn request_stream(&self, req: Payload) -> Flux<Payload> {
        let inner = self.inner.read().unwrap();
        (*inner).request_stream(req)
    }
}