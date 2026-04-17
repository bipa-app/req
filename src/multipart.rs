use futures::{ready, Stream};
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct Body(common_multipart_rfc7578::client::multipart::Body<'static>);

impl From<common_multipart_rfc7578::client::multipart::Body<'static>> for Body {
    #[inline]
    fn from(body: common_multipart_rfc7578::client::multipart::Body<'static>) -> Self {
        Body(body)
    }
}

impl hyper::body::Body for Body {
    type Data = hyper::body::Bytes;
    type Error = common_multipart_rfc7578::client::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let Body(inner) = Pin::into_inner(self);

        match ready!(Pin::new(inner).poll_next(cx)) {
            Some(Ok(read)) => Poll::Ready(Some(Ok(hyper::body::Frame::data(read.freeze())))),
            Some(Err(e)) => Poll::Ready(Some(Err(e))),
            None => Poll::Ready(None),
        }
    }
}
