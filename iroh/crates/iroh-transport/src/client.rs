//! Client side of transport v1: connects to a host, authenticates, and issues HTTP requests.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use iroh::endpoint::{Connection, RecvStream, SendStream, VarInt};
use iroh::{Endpoint, EndpointAddr};
use tokio::sync::Mutex;
use tokio::time::{Instant, timeout};

use crate::framing::{read_frame, write_frame};
use crate::net::{PathReport, current_path};
use crate::policy::TransportPolicy;
use crate::protocol::{
    ALPN, AuthOutcome, AuthRequest, BodyMode, Header, Hello, HttpRequest, HttpResponse,
    MAX_CONTROL_FRAME_BYTES, MAX_HTTP_FRAME_BYTES, Message, PROTOCOL_VERSION, Ping, close_code,
    error_code, validate_http_request, validate_http_response,
};

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Longer than the host's upstream timeout so the host's own error arrives first.
pub const RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("could not connect to the host: {0}")]
    Connect(String),
    #[error("the host refused access: {0}")]
    Denied(String),
    #[error("the host sent an unexpected reply: {0}")]
    Protocol(String),
    #[error("request failed ({code}): {message}")]
    Request { code: String, message: String },
    #[error("connection problem: {0}")]
    Connection(String),
    #[error("timed out waiting for the host")]
    Timeout,
}

pub struct ClientResponse {
    pub head: HttpResponse,
    /// The response body. Read until end of stream; drop it to cancel the request.
    pub body: RecvStream,
}

pub struct ClientSession {
    connection: Connection,
    /// Held open for the session's lifetime; finishing it ends the session.
    control: Mutex<(SendStream, RecvStream)>,
    outcome: AuthOutcome,
    transport_policy: TransportPolicy,
    next_request: AtomicU64,
}

impl ClientSession {
    pub async fn connect(
        endpoint: &Endpoint,
        addr: EndpointAddr,
        device_name: &str,
        pairing_token: Option<&str>,
    ) -> Result<Self, ClientError> {
        let connection = endpoint
            .connect(addr, ALPN)
            .await
            .map_err(|error| ClientError::Connect(error.to_string()))?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| ClientError::Connection(error.to_string()))?;
        let hello = Message::Hello(Hello {
            protocol: PROTOCOL_VERSION,
            name: device_name.to_owned(),
            capabilities: Vec::new(),
        });
        let auth = Message::AuthRequest(AuthRequest {
            protocol: PROTOCOL_VERSION,
            pairing_token: pairing_token.map(str::to_owned),
        });
        for message in [hello, auth] {
            write_frame(&mut send, &message, MAX_CONTROL_FRAME_BYTES)
                .await
                .map_err(|error| ClientError::Connection(error.to_string()))?;
        }
        let reply = timeout(
            HANDSHAKE_TIMEOUT,
            read_frame(&mut recv, MAX_CONTROL_FRAME_BYTES),
        )
        .await
        .map_err(|_| ClientError::Timeout)?
        .map_err(|error| ClientError::Protocol(error.to_string()))?;
        let Message::AuthResult(result) = reply else {
            return Err(ClientError::Protocol("expected AUTH_RESULT".into()));
        };
        if result.outcome == AuthOutcome::Denied {
            connection.close(VarInt::from_u32(close_code::NORMAL), b"denied");
            return Err(ClientError::Denied(
                result
                    .reason
                    .unwrap_or_else(|| error_code::NOT_AUTHORIZED.into()),
            ));
        }
        Ok(Self {
            connection,
            control: Mutex::new((send, recv)),
            outcome: result.outcome,
            transport_policy: result.transport_policy,
            next_request: AtomicU64::new(1),
        })
    }

    pub fn outcome(&self) -> AuthOutcome {
        self.outcome
    }

    pub fn transport_policy(&self) -> TransportPolicy {
        self.transport_policy
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn path(&self) -> PathReport {
        current_path(&self.connection)
    }

    /// Application-level round trip over the control stream.
    pub async fn ping(&self) -> Result<Duration, ClientError> {
        let mut control = self.control.lock().await;
        let (send, recv) = &mut *control;
        let nonce: u64 = rand::random();
        let started = Instant::now();
        write_frame(send, &Message::Ping(Ping { nonce }), MAX_CONTROL_FRAME_BYTES)
            .await
            .map_err(|error| ClientError::Connection(error.to_string()))?;
        let reply = timeout(HANDSHAKE_TIMEOUT, read_frame(recv, MAX_CONTROL_FRAME_BYTES))
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(|error| ClientError::Connection(error.to_string()))?;
        match reply {
            Message::Pong(pong) if pong.nonce == nonce => Ok(started.elapsed()),
            _ => Err(ClientError::Protocol("expected matching PONG".into())),
        }
    }

    /// Sends one HTTP request on its own stream and returns once response headers arrive.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        headers: Vec<Header>,
        body: Option<Bytes>,
    ) -> Result<ClientResponse, ClientError> {
        let request = HttpRequest {
            request_id: format!("r{}", self.next_request.fetch_add(1, Ordering::Relaxed)),
            method: method.to_owned(),
            path: path.to_owned(),
            headers,
            body: if body.is_some() {
                BodyMode::Stream
            } else {
                BodyMode::None
            },
            content_length: body.as_ref().map(|bytes| bytes.len() as u64),
        };
        validate_http_request(&request).map_err(|problem| ClientError::Request {
            code: error_code::BAD_REQUEST.into(),
            message: problem.into(),
        })?;

        let (mut send, mut recv) = self
            .connection
            .open_bi()
            .await
            .map_err(|error| ClientError::Connection(error.to_string()))?;
        write_frame(
            &mut send,
            &Message::HttpRequest(request),
            MAX_HTTP_FRAME_BYTES,
        )
        .await
        .map_err(|error| ClientError::Connection(error.to_string()))?;
        if let Some(body) = body {
            send.write_chunk(body)
                .await
                .map_err(|error| ClientError::Connection(error.to_string()))?;
        }
        send.finish()
            .map_err(|error| ClientError::Connection(error.to_string()))?;

        let reply = timeout(
            RESPONSE_HEADER_TIMEOUT,
            read_frame(&mut recv, MAX_HTTP_FRAME_BYTES),
        )
        .await
        .map_err(|_| ClientError::Timeout)?
        .map_err(|error| ClientError::Protocol(error.to_string()))?;
        match reply {
            Message::HttpResponse(head) => {
                validate_http_response(&head)
                    .map_err(|problem| ClientError::Protocol(problem.into()))?;
                Ok(ClientResponse { head, body: recv })
            }
            Message::StreamError(error) => Err(ClientError::Request {
                code: error.code,
                message: error.message,
            }),
            _ => Err(ClientError::Protocol("expected HTTP_RESPONSE".into())),
        }
    }

    /// Ends the session cleanly.
    pub async fn close(self) {
        let (mut send, _recv) = self.control.into_inner();
        let _ = send.finish();
        let _ = timeout(Duration::from_secs(2), self.connection.closed()).await;
        self.connection
            .close(VarInt::from_u32(close_code::NORMAL), b"bye");
    }
}
