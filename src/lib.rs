mod multipart;

pub use common_multipart_rfc7578::client::multipart::Form;
pub use hyper::{Method, StatusCode, Uri, body::Bytes};
use hyper_util::client::legacy::ResponseFuture;
pub use mime;
pub use opentelemetry::{
    Context, KeyValue,
    trace::{Span, Tracer},
};

use http_body_util::BodyExt;
use hyper::Request;
use hyper_rustls::ConfigBuilderExt;
use opentelemetry::{
    global::BoxedSpan,
    trace::{FutureExt, TraceContextExt},
};
use opentelemetry_semantic_conventions::{
    resource::SERVICE_NAME,
    trace::{HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, HTTP_ROUTE},
};
use rustls::ClientConfig;
use serde::Serialize;
use std::future::Future;

/// Default per-phase timeout: the most time spent awaiting the response
/// headers, and the longest gap allowed between two response-body chunks.
/// Bounds a stalled peer without capping the total time of a legitimately
/// large download. Override per client with [`Client::with_timeout`].
const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Clone)]
pub struct Client {
    pub uri: Uri,
    pub name: &'static str,
    timeout: std::time::Duration,
    hyper: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::combinators::BoxBody<Bytes, BodyError>,
    >,
    duration: opentelemetry::metrics::Histogram<u64>,
    pub tracer: std::sync::Arc<opentelemetry::global::BoxedTracer>,
}

impl Client {
    /// Sets the per-phase timeout: the longest the request may wait for the
    /// response headers, and the longest gap allowed between response-body
    /// chunks. Bounds a stalled peer without capping total download time.
    /// Defaults to 10 seconds.
    #[must_use]
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Body could not be bodied")]
pub struct BodyError;

#[must_use]
pub fn client(name: &'static str, uri: Uri) -> Client {
    let config = ClientConfig::builder()
        .with_webpki_roots()
        .with_no_client_auth();

    let tls = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();

    let hyper = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(tls);

    let duration = opentelemetry::global::meter(name)
        .u64_histogram("http.client.request.duration")
        .with_description("How much time does it take to make the request?")
        .with_unit("ms")
        .build();

    let tracer = std::sync::Arc::new(opentelemetry::global::tracer(name));

    Client {
        uri,
        name,
        timeout: DEFAULT_TIMEOUT,
        hyper,
        duration,
        tracer,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("prepare: {0:?}")]
    Prepare(hyper::http::Error),
    #[error("encode json: {0:?}")]
    EncodeJson(serde_json::Error),
    #[error("encode form: {0:?}")]
    EncodeForm(serde_urlencoded::ser::Error),
    #[error("encode network: {0:?}")]
    Network(hyper_util::client::legacy::Error),
    #[error("read: {0:?}")]
    Read(hyper::Error),
    #[error("timeout")]
    Timeout,
}

fn res_process(
    c: &Client,
    span: BoxedSpan,
    target: &'static str,
    method: Method,
    request: Result<ResponseFuture, Error>,
) -> impl Future<Output = Result<(StatusCode, Bytes), Error>> + use<> {
    let service_name = c.name;
    let duration = c.duration.clone();
    let timeout = c.timeout;
    let instant = std::time::Instant::now();

    async move {
        let ctx = opentelemetry::Context::current();
        let span = ctx.span();

        // Bound each phase independently — awaiting the response headers, and
        // each gap between body chunks — so a stalled peer can't hold the
        // request open forever, without capping the total time of a
        // legitimately large download.
        let result = async move {
            let response = match tokio::time::timeout(timeout, request?).await {
                Ok(response) => response.map_err(Error::Network)?,
                Err(_elapsed) => return Err(Error::Timeout),
            };

            let status = response.status();
            let mut body = response.into_body();
            let mut buf = bytes::BytesMut::new();
            loop {
                match tokio::time::timeout(timeout, body.frame()).await {
                    Ok(Some(frame)) => {
                        if let Ok(data) = frame.map_err(Error::Read)?.into_data() {
                            buf.extend_from_slice(&data);
                        }
                    }
                    Ok(None) => break,
                    Err(_elapsed) => return Err(Error::Timeout),
                }
            }

            Ok((status, buf.freeze()))
        }
        .await;

        let elapsed = u64::try_from(instant.elapsed().as_millis()).unwrap_or(u64::MAX);
        match &result {
            Ok((status, _)) => {
                let code = status.as_u16().to_string();
                span.set_attribute(KeyValue::new(HTTP_RESPONSE_STATUS_CODE, code.clone()));
                span.set_status(opentelemetry::trace::Status::Ok);
                duration.record(
                    elapsed,
                    &[
                        KeyValue::new(SERVICE_NAME, service_name),
                        KeyValue::new(HTTP_RESPONSE_STATUS_CODE, code),
                        KeyValue::new(HTTP_REQUEST_METHOD, method.as_str().to_string()),
                        KeyValue::new(HTTP_ROUTE, target),
                    ],
                );
            }
            Err(e) => {
                span.set_status(opentelemetry::trace::Status::Error {
                    description: e.to_string().into(),
                });
                duration.record(
                    elapsed,
                    &[
                        KeyValue::new(SERVICE_NAME, service_name),
                        KeyValue::new(HTTP_REQUEST_METHOD, method.as_str().to_string()),
                        KeyValue::new(HTTP_ROUTE, target),
                    ],
                );
            }
        }

        result
    }
    .with_context(opentelemetry::Context::current_with_span(span))
}

pub fn req(
    c: &Client,
    span: BoxedSpan,
    target: &'static str,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> impl Future<Output = Result<(StatusCode, Bytes), Error>> + use<> {
    let mut request = Request::builder();

    for &(hn, hv) in headers {
        request = request.header(hn, hv);
    }

    let body = match body {
        None => http_body_util::Empty::new().map_err(|_| BodyError).boxed(),
        Some(body) => http_body_util::Full::from(body)
            .map_err(|_| BodyError)
            .boxed(),
    };

    let request = request
        .method(&method)
        .uri(uri)
        .body(body)
        .map_err(Error::Prepare)
        .map(|request| c.hyper.request(request));

    res_process(c, span, target, method, request)
}

pub fn req_json<T: Serialize>(
    c: &Client,
    span: BoxedSpan,
    target: &'static str,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: T,
) -> impl Future<Output = Result<(StatusCode, Bytes), Error>> + use<T> {
    let request = serde_json::to_vec(&body)
        .map_err(Error::EncodeJson)
        .and_then(|body| {
            let mut request = Request::builder();
            request = request.header(hyper::header::CONTENT_TYPE, "application/json");

            for &(hn, hv) in headers {
                request = request.header(hn, hv);
            }

            request
                .method(&method)
                .uri(uri)
                .body(
                    http_body_util::Full::from(body)
                        .map_err(|_| BodyError)
                        .boxed(),
                )
                .map_err(Error::Prepare)
                .map(|request| c.hyper.request(request))
        });

    res_process(c, span, target, method, request)
}

pub fn req_form_urlencoded<T: Serialize>(
    c: &Client,
    span: BoxedSpan,
    target: &'static str,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: T,
) -> impl Future<Output = Result<(StatusCode, Bytes), Error>> + use<T> {
    let request = serde_urlencoded::to_string(body)
        .map_err(Error::EncodeForm)
        .and_then(|body| {
            let mut request = Request::builder();
            request = request.header(
                hyper::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            );

            for &(hn, hv) in headers {
                request = request.header(hn, hv);
            }

            request
                .method(&method)
                .uri(uri)
                .body(body.map_err(|_| BodyError).boxed())
                .map_err(Error::Prepare)
                .map(|request| c.hyper.request(request))
        });

    res_process(c, span, target, method, request)
}

pub fn req_form_multipart(
    c: &Client,
    span: BoxedSpan,
    target: &'static str,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    form: Form<'static>,
) -> impl Future<Output = Result<(StatusCode, Bytes), Error>> + use<> {
    let mut request = Request::builder();
    request = request.header(hyper::header::CONTENT_TYPE, form.content_type());

    for &(hn, hv) in headers {
        request = request.header(hn, hv);
    }

    let body = multipart::Body::from(common_multipart_rfc7578::client::multipart::Body::from(
        form,
    ));

    let request = request
        .method(&method)
        .uri(uri)
        .body(body.map_err(|_| BodyError).boxed())
        .map_err(Error::Prepare)
        .map(|request| c.hyper.request(request));

    res_process(c, span, target, method, request)
}

#[macro_export]
macro_rules! req {
    (
        $client:expr;
        $method:ident, $path:literal, $($arg:expr),*;
        $($hn:expr=>$hv:expr),*
    ) => {{
        let (span, target, url) = $crate::span!($client; $method, $path, $($arg),*);
        $crate::req(&$client, span, target, $crate::Method::$method, &url, &[$(($hn, $hv)),*], None)
    }};
    (
        $client:expr;
        $method:ident, $path:literal, $($arg:expr),*;
        $($hn:expr=>$hv:expr),*;
        $body:expr
    ) => {{
        let (span, target, url) = $crate::span!($client; $method, $path, $($arg),*);
        $crate::req(&$client, span, target, $crate::Method::$method, &url, &[$(($hn, $hv)),*], $body)
    }};
    (
        $client:expr;
        $method:ident, $path:literal, $($arg:expr),*;
        $($hn:expr=>$hv:expr),*;
        json: $body:expr
    ) => {{
        let (span, target, url) = $crate::span!($client; $method, $path, $($arg),*);
        $crate::req_json(&$client, span, target, $crate::Method::$method, &url, &[$(($hn, $hv)),*], $body)
    }};
    (
        $client:expr;
        $method:ident, $path:literal, $($arg:expr),*;
        $($hn:expr=>$hv:expr),*;
        form/multipart: $body:expr
    ) => {{
        let (span, target, url) = $crate::span!($client; $method, $path, $($arg),*);
        $crate::req_form_multipart(&$client, span, target, $crate::Method::$method, &url, &[$(($hn, $hv)),*], $body)
    }};
    (
        $client:expr;
        $method:ident, $path:literal, $($arg:expr),*;
        $($hn:expr=>$hv:expr),*;
        form/urlencoded: $body:expr
    ) => {{
        let (span, target, url) = $crate::span!($client; $method, $path, $($arg),*);
        $crate::req_form_urlencoded(&$client, span, target, $crate::Method::$method, &url, &[$(($hn, $hv)),*], $body)
    }};
}

#[macro_export]
macro_rules! span {
    ($client:expr; $method:ident, $target:literal, $($arg:expr),*) => {{
        use $crate::{Tracer, Span};

        let url = format!("{}{}", $client.uri, format!($target, $($arg),*));
        let mut span = $client.tracer.start(concat!(stringify!($method), " ", $target));

        span.set_attributes([
            $crate::KeyValue::new("peer.service", $client.name),
            $crate::KeyValue::new("url.full", url.clone()),
            $crate::KeyValue::new("http.route", $target),
            $crate::KeyValue::new("http.request.method", stringify!($method)),
        ]);

        (span, $target, url)
    }};
}

#[cfg(test)]
mod test {

    #[test]
    fn macro_signatures() {
        use super::{Form, client};

        let client = client("test", hyper::Uri::from_static("/uri"));

        // no body
        drop(req!(client; GET, "/oi/{}", "blz"; "auth" => "yo"));

        // bare body
        drop(
            req!(client; GET, "/oi/{}", "blz"; "auth" => "yo"; Some("body".as_bytes().to_owned())),
        );

        // json
        drop(req!(client; POST, "/oi/{}", "blz"; "auth" => "yo"; json: "serializable"));

        // form multipart
        drop(req!(client; PUT, "/oi/{}", "blz"; "auth" => "yo"; form/multipart: Form::default()));

        // form urlencoded
        drop(req!(client; PATCH, "/oi/{}", "blz"; "auth" => "yo"; form/urlencoded: ("oi", "blz")));
    }

    #[test]
    fn with_timeout_overrides_default() {
        use super::{DEFAULT_TIMEOUT, client};

        let client = client("test", hyper::Uri::from_static("/uri"));
        assert_eq!(client.timeout, DEFAULT_TIMEOUT);

        let client = client.with_timeout(std::time::Duration::from_secs(120));
        assert_eq!(client.timeout, std::time::Duration::from_secs(120));
    }
}
