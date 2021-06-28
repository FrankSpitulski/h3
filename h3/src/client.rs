use bytes::{Bytes, BytesMut};
use futures::future;
use http::{request, HeaderMap, Response};
use std::{
    convert::TryFrom,
    marker::PhantomData,
    task::{Context, Poll},
};

use crate::{
    connection::{self, ConnectionInner, SharedStateRef},
    error::{Code, Error},
    frame::FrameStream,
    proto::{frame::Frame, headers::Header, varint::VarInt},
    qpack, quic, stream,
};
use tracing::{trace, warn};

pub fn builder<C: quic::Connection<Bytes>>() -> Builder<C> {
    Builder::new()
}

pub async fn new<C, O>(conn: C) -> Result<(Connection<C>, SendRequest<C::OpenStreams>), Error>
where
    C: quic::Connection<Bytes, OpenStreams = O>,
    O: quic::OpenStreams<Bytes>,
{
    Ok(Builder::new().build(conn).await?)
}

pub struct SendRequest<T: quic::OpenStreams<Bytes>> {
    open: T,
    conn_state: SharedStateRef,
    max_field_section_size: u64, // maximum size for a header we receive
}

impl<T> SendRequest<T>
where
    T: quic::OpenStreams<Bytes>,
{
    pub async fn send_request(
        &mut self,
        req: http::Request<()>,
    ) -> Result<RequestStream<FrameStream<T::BidiStream>>, Error> {
        let peer_max_field_section_size = {
            let state = self.conn_state.0.read().expect("send request lock state");
            state.peer_max_field_section_size
        };

        let (parts, _) = req.into_parts();
        let request::Parts {
            method,
            uri,
            headers,
            ..
        } = parts;
        let headers = Header::request(method, uri, headers)?;

        let mut stream =
            future::poll_fn(|cx| self.open.poll_open_bidi(cx).map_err(Error::transport)).await?;

        let mut block = BytesMut::new();
        let mem_size = qpack::encode_stateless(&mut block, headers)?;
        if mem_size > peer_max_field_section_size {
            return Err(Error::header_too_big(mem_size, peer_max_field_section_size));
        }

        stream::write(&mut stream, Frame::Headers(block.freeze())).await?;

        Ok(RequestStream {
            inner: connection::RequestStream::new(
                FrameStream::new(stream),
                self.max_field_section_size,
                self.conn_state.clone(),
            ),
        })
    }

    #[cfg(feature = "test_helpers")]
    pub fn state(&self) -> SharedStateRef {
        self.conn_state.clone()
    }
}

pub struct Connection<C>
where
    C: quic::Connection<Bytes>,
{
    inner: ConnectionInner<C>,
}

impl<C> Connection<C>
where
    C: quic::Connection<Bytes>,
{
    pub fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        while let Poll::Ready(frame) = self.inner.poll_control(cx)? {
            match frame {
                Frame::Settings(_) => trace!("Got settings"),
                f @ Frame::Goaway(_) => {
                    warn!("Control frame ignored {:?}", f);
                }
                frame => {
                    return Poll::Ready(Err(Code::H3_FRAME_UNEXPECTED
                        .with_reason(format!("on client control stream: {:?}", frame))))
                }
            }
        }

        if let Poll::Ready(_) = self.inner.poll_accept_request(cx) {
            return Poll::Ready(Err(self.inner.close(
                Code::H3_STREAM_CREATION_ERROR,
                "client received a bidirectionnal stream",
            )));
        }

        Poll::Pending
    }
}

pub struct Builder<C>
where
    C: quic::Connection<Bytes>,
{
    pub(super) max_field_section_size: u64,
    _conn: PhantomData<C>,
}

impl<C, O> Builder<C>
where
    C: quic::Connection<Bytes, OpenStreams = O>,
    O: quic::OpenStreams<Bytes>,
{
    pub(super) fn new() -> Self {
        Builder {
            max_field_section_size: VarInt::MAX.0,
            _conn: PhantomData,
        }
    }

    pub fn max_field_section_size(&mut self, value: u64) -> &mut Self {
        self.max_field_section_size = value;
        self
    }

    pub async fn build(&mut self, quic: C) -> Result<(Connection<C>, SendRequest<O>), Error> {
        let open = quic.opener();
        let conn_state = SharedStateRef::default();

        Ok((
            Connection {
                inner: ConnectionInner::new(quic, self.max_field_section_size, conn_state.clone())
                    .await?,
            },
            SendRequest {
                open,
                conn_state,
                max_field_section_size: self.max_field_section_size,
            },
        ))
    }
}

pub struct RequestStream<S> {
    inner: connection::RequestStream<S, Bytes>,
}

impl<S> RequestStream<FrameStream<S>>
where
    S: quic::RecvStream,
{
    pub async fn recv_response(&mut self) -> Result<Response<()>, Error> {
        let mut frame = future::poll_fn(|cx| self.inner.stream.poll_next(cx))
            .await?
            .ok_or_else(|| {
                Code::H3_GENERAL_PROTOCOL_ERROR.with_reason("Did not receive response headers")
            })?;

        let (fields, mem_size) = if let Frame::Headers(ref mut encoded) = frame {
            qpack::decode_stateless(encoded)?
        } else {
            return Err(
                Code::H3_FRAME_UNEXPECTED.with_reason("First response frame is not headers")
            );
        };

        if mem_size > self.inner.max_field_section_size {
            self.inner.stop_sending(Code::H3_REQUEST_REJECTED);
            return Err(Error::header_too_big(
                mem_size,
                self.inner.max_field_section_size,
            ));
        }

        let (status, headers) = Header::try_from(fields)?.into_response_parts()?;
        let mut resp = Response::new(());
        *resp.status_mut() = status;
        *resp.headers_mut() = headers;
        *resp.version_mut() = http::Version::HTTP_3;

        Ok(resp)
    }

    pub async fn recv_data(&mut self) -> Result<Option<Bytes>, Error> {
        self.inner.recv_data().await
    }

    pub async fn recv_trailers(&mut self) -> Result<Option<HeaderMap>, Error> {
        let res = self.inner.recv_trailers().await;
        if let Err(ref e) = res {
            if let crate::error::Kind::HeaderTooBig { .. } = e.kind() {
                self.inner.stream.stop_sending(Code::H3_REQUEST_CANCELLED);
            }
        }
        res
    }

    pub fn stop_sending(&mut self, error_code: crate::error::Code) {
        self.inner.stream.stop_sending(error_code)
    }
}

impl<S> RequestStream<S>
where
    S: quic::SendStream<Bytes>,
{
    pub async fn send_data(&mut self, buf: Bytes) -> Result<(), Error> {
        self.inner.send_data(buf).await
    }

    pub async fn send_trailers(&mut self, trailers: HeaderMap) -> Result<(), Error> {
        self.inner.send_trailers(trailers).await
    }

    pub async fn finish(&mut self) -> Result<(), Error> {
        self.inner.finish().await
    }
}
