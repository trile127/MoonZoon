use crate::actor::{sessions, Index};
use actix_web::web::Bytes;
use actix_web::{rt, Error};
use chashmap::CHashMap;
use futures::Stream;
use moonlight::SessionId;
use std::cell::RefCell;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc::{error::SendError, unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::{interval_at, Instant};

pub type ShareableSSE = Arc<SSE>;

// ------ Connection ------

pub struct Connection {
    remove_session_actor_on_remove: bool,
    session_id: SessionId,
    sender: UnboundedSender<Bytes>,
}

impl Connection {
    fn new(session_id: Option<SessionId>) -> (Arc<Connection>, EventStream) {
        let (sender, receiver) = unbounded_channel();
        let connection = Arc::new(Self {
            remove_session_actor_on_remove: session_id.is_some(),
            session_id: session_id.unwrap_or_else(SessionId::new),
            sender,
        });
        (connection, EventStream(receiver))
    }

    fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub fn send(&self, event: &str, data: &str) -> Result<(), SendError<Bytes>> {
        let message = Bytes::from(["event: ", event, "\n", "data: ", data, "\n\n"].concat());
        self.sender.send(message)
    }
}

// ------ EventStream ------

pub struct EventStream(UnboundedReceiver<Bytes>);

impl Stream for EventStream {
    type Item = Result<Bytes, Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.0).poll_recv(cx) {
            Poll::Ready(Some(bytes)) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ------ SSE ------

pub struct SSE {
    connections: CHashMap<SessionId, Arc<Connection>>,
}

impl SSE {
    pub fn start() -> ShareableSSE {
        let sse = SSE {
            connections: CHashMap::new(),
        };
        let this = Arc::new(sse);
        this.spawn_connection_remover();
        this
    }
}

// ------ ShareableSSEMethods ------

pub trait ShareableSSEMethods {
    fn spawn_connection_remover(&self);

    fn new_connection(&self, session_id: Option<SessionId>) -> (Arc<Connection>, EventStream);

    fn broadcast(&self, event: &str, data: &str) -> Result<(), Vec<SendError<Bytes>>>;

    fn send(
        &self,
        session_id: &SessionId,
        event: &str,
        data: &str,
    ) -> Option<Result<(), SendError<Bytes>>>;

    // @TODO why is it a dead code since Rust 1.78.0?
    #[allow(dead_code)]
    fn remove_connection(&self, session_id: &SessionId);
}

impl ShareableSSEMethods for ShareableSSE {
    fn spawn_connection_remover(&self) {
        let this = self.clone();
        rt::spawn(async move {
            let mut interval = interval_at(Instant::now(), Duration::from_secs(10));
            loop {
                interval.tick().await;
                this.connections.retain(|session_id, connection| {
                    let active = connection.send("ping", "").is_ok();
                    if !active && connection.remove_session_actor_on_remove {
                        if let Some(session_actor) = sessions::by_session_id().get(session_id) {
                            session_actor.remove();
                        }
                    }
                    active
                });
            }
        });
    }

    fn new_connection(&self, session_id: Option<SessionId>) -> (Arc<Connection>, EventStream) {
        let (connection, event_stream) = Connection::new(session_id);
        self.connections
            .insert(connection.session_id(), connection.clone());
        (connection, event_stream)
    }

    fn broadcast(&self, event: &str, data: &str) -> Result<(), Vec<SendError<Bytes>>> {
        let errors = RefCell::new(Vec::new());
        self.connections.retain(|_, connection| {
            if let Err(error) = connection.send(event, data) {
                errors.borrow_mut().push(error);
            }
            true
        });
        let errors = errors.into_inner();
        if errors.is_empty() {
            return Ok(());
        }
        Err(errors)
    }

    fn send(
        &self,
        session_id: &SessionId,
        event: &str,
        data: &str,
    ) -> Option<Result<(), SendError<Bytes>>> {
        // @TODO Last-Event-Id
        self.connections
            .get(session_id)
            .map(|connection| connection.send(event, data))
    }

    fn remove_connection(&self, session_id: &SessionId) {
        let connection = self.connections.remove(session_id);

        if let Some(connection) = connection {
            if connection.remove_session_actor_on_remove {
                if let Some(session_actor) = sessions::by_session_id().get(session_id) {
                    session_actor.remove();
                }
            }
        }
    }
}
