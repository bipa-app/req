mod multipart;

pub use common_multipart_rfc7578::client::multipart::Form;
pub use hyper::{body::Bytes, Method, StatusCode, Uri};
pub use mime;
pub use opentelemetry::{
    trace::{Span, Tracer},
    Context, KeyValue,
};

use http_body_util::BodyExt;
use hyper::Request;
use hyper_rustls::ConfigBuilderExt;
use opentelemetry::{
    global::BoxedSpan,
    metrics::{Counter, Histogram},
    trace::{FutureExt, TraceContextExt},
};
use opentelemetry_semantic_conventions::{
    resource::SERVICE_NAME,
    trace::{HTTP_REQUEST_METHOD, HTTP_RESPONSE_STATUS_CODE, HTTP_ROUTE},
};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    ClientConfig,
};
use serde::Serialize;
use std::future::Future;

#[derive(Clone)]
pub struct Client {
    pub uri: Uri,
    pub name: &'static str,
    hyper: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::combinators::BoxBody<Bytes, BodyError>,
    >,
    metrics: Metrics,
    pub tracer: std::sync::Arc<opentelemetry::global::BoxedTracer>,
}

#[derive(Debug, thiserror::Error)]
#[error("Body could not be bodied")]
pub struct BodyError;

#[derive(Clone)]
struct Metrics {
    count: Counter<u64>,
    duration: Histogram<u64>,
}

#[must_use]
pub fn client(name: &'static str, uri: Uri) -> Client {
    let config = ClientConfig::builder()
        .with_webpki_roots()
        .with_no_client_auth();

    let tls = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_only()
        .enable_http1()
        .enable_http2()
        .build();

    let hyper = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(tls);

    let metrics = metrics(&opentelemetry::global::meter(name));
    let tracer = std::sync::Arc::new(opentelemetry::global::tracer(name));

    Client {
        uri,
        name,
        hyper,
        metrics,
        tracer,
    }
}

pub fn client_mtls(
    name: &'static str,
    uri: Uri,
    key: PrivateKeyDer<'static>,
    certs: Vec<CertificateDer<'static>>,
) -> Result<Client, rustls::Error> {
    let config = ClientConfig::builder()
        .with_webpki_roots()
        .with_client_auth_cert(certs, key)?;

    let tls = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(config)
        .https_only()
        .enable_http1()
        .enable_http2()
        .build();

    let hyper = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(tls);

    let metrics = metrics(&opentelemetry::global::meter(name));
    let tracer = std::sync::Arc::new(opentelemetry::global::tracer(name));

    Ok(Client {
        uri,
        name,
        hyper,
        metrics,
        tracer,
    })
}

fn metrics(meter: &opentelemetry::metrics::Meter) -> Metrics {
    let count = meter
        .u64_counter("http.client.request.count")
        .with_description("How many requests are we making?")
        .init();

    let duration = meter
        .u64_histogram("http.client.request.duration")
        .with_description("How much time does it take to make the request?")
        .with_unit("ms")
        .init();

    Metrics { count, duration }
}

#[derive(Debug)]
pub enum Error {
    Prepare(hyper::http::Error),
    EncodeJson(serde_json::Error),
    EncodeForm(serde_urlencoded::ser::Error),
    Network(hyper_util::client::legacy::Error),
    Read(hyper::Error),
}

pub fn req(
    c: &Client,
    span: BoxedSpan,
    target: &'static str,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> impl Future<Output = Result<(StatusCode, Bytes), Error>> {
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

    let service_name = c.name;
    let metrics = c.metrics.clone();
    let instant = std::time::Instant::now();

    async move {
        let response = request?.await.map_err(Error::Network)?;

        let status = response.status();
        let ctx = opentelemetry::Context::current();
        ctx.span().set_attribute(opentelemetry::KeyValue::new(
            HTTP_RESPONSE_STATUS_CODE,
            status.to_string(),
        ));

        let attrs = [
            KeyValue::new(SERVICE_NAME, service_name),
            KeyValue::new(HTTP_RESPONSE_STATUS_CODE, status.as_str().to_string()),
            KeyValue::new(HTTP_REQUEST_METHOD, method.as_str().to_string()),
            KeyValue::new(HTTP_ROUTE, target),
        ];

        let duration = u64::try_from(instant.elapsed().as_millis()).unwrap_or(u64::MAX);
        metrics.duration.record(duration, &attrs);
        metrics.count.add(1, &attrs);

        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(Error::Read)?
            .to_bytes();
        Ok((status, bytes))
    }
    .with_context(opentelemetry::Context::current_with_span(span))
}

pub fn req_json<T: Serialize>(
    c: &Client,
    span: BoxedSpan,
    target: &'static str,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: T,
) -> impl Future<Output = Result<(StatusCode, Bytes), Error>> {
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

    let service_name = c.name;
    let count = c.metrics.count.clone();

    async move {
        let response = request?.await.map_err(Error::Network)?;

        let status = response.status();
        let ctx = opentelemetry::Context::current();
        ctx.span().set_attribute(opentelemetry::KeyValue::new(
            HTTP_RESPONSE_STATUS_CODE,
            status.to_string(),
        ));

        count.add(
            1,
            &[
                KeyValue::new(SERVICE_NAME, service_name),
                KeyValue::new(HTTP_RESPONSE_STATUS_CODE, status.as_str().to_string()),
                KeyValue::new(HTTP_REQUEST_METHOD, method.as_str().to_string()),
                KeyValue::new(HTTP_ROUTE, target),
            ],
        );

        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(Error::Read)?
            .to_bytes();
        Ok((status, bytes))
    }
    .with_context(opentelemetry::Context::current_with_span(span))
}

pub fn req_form_urlencoded<T: Serialize>(
    c: &Client,
    span: BoxedSpan,
    target: &'static str,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: T,
) -> impl Future<Output = Result<(StatusCode, Bytes), Error>> {
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

    let service_name = c.name;
    let count = c.metrics.count.clone();

    async move {
        let response = request?.await.map_err(Error::Network)?;

        let status = response.status();
        let ctx = opentelemetry::Context::current();
        ctx.span().set_attribute(opentelemetry::KeyValue::new(
            HTTP_RESPONSE_STATUS_CODE,
            status.to_string(),
        ));

        count.add(
            1,
            &[
                KeyValue::new(SERVICE_NAME, service_name),
                KeyValue::new(HTTP_RESPONSE_STATUS_CODE, status.as_str().to_string()),
                KeyValue::new(HTTP_REQUEST_METHOD, method.as_str().to_string()),
                KeyValue::new(HTTP_ROUTE, target),
            ],
        );

        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(Error::Read)?
            .to_bytes();
        Ok((status, bytes))
    }
    .with_context(opentelemetry::Context::current_with_span(span))
}

pub fn req_form_multipart(
    c: &Client,
    span: BoxedSpan,
    target: &'static str,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    form: Form<'static>,
) -> impl Future<Output = Result<(StatusCode, Bytes), Error>> {
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

    let service_name = c.name;
    let count = c.metrics.count.clone();

    async move {
        let request = request?;
        let response = request.await.map_err(Error::Network)?;

        let status = response.status();
        let ctx = opentelemetry::Context::current();
        ctx.span().set_attribute(opentelemetry::KeyValue::new(
            HTTP_RESPONSE_STATUS_CODE,
            status.to_string(),
        ));

        count.add(
            1,
            &[
                KeyValue::new(SERVICE_NAME, service_name),
                KeyValue::new(HTTP_RESPONSE_STATUS_CODE, status.as_str().to_string()),
                KeyValue::new(HTTP_REQUEST_METHOD, method.as_str().to_string()),
                KeyValue::new(HTTP_ROUTE, target),
            ],
        );

        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(Error::Read)?
            .to_bytes();
        Ok((status, bytes))
    }
    .with_context(opentelemetry::Context::current_with_span(span))
}

#[macro_export]
macro_rules! req {
    (
        $client:expr$(, $span:expr)?;
        $method:ident, $path:literal, $($arg:expr),*;
        $($hn:expr=>$hv:expr),*
    ) => {{
        let (span, target, url) = $crate::span!($client$(, $span)?; $method, $path, $($arg),*);
        $crate::req(&$client, span, target, $crate::Method::$method, &url, &[$(($hn, $hv)),*], None)
    }};
    (
        $client:expr$(, $span:expr)?;
        $method:ident, $path:literal, $($arg:expr),*;
        $($hn:expr=>$hv:expr),*;
        $body:expr
    ) => {{
        let (span, target, url) = $crate::span!($client$(, $span)?; $method, $path, $($arg),*);
        $crate::req(&$client, span, target, $crate::Method::$method, &url, &[$(($hn, $hv)),*], $body)
    }};
    (
        $client:expr$(, $span:expr)?;
        $method:ident, $path:literal, $($arg:expr),*;
        $($hn:expr=>$hv:expr),*;
        json: $body:expr
    ) => {{
        let (span, target, url) = $crate::span!($client$(, $span)?; $method, $path, $($arg),*);
        $crate::req_json(&$client, span, target, $crate::Method::$method, &url, &[$(($hn, $hv)),*], $body)
    }};
    (
        $client:expr$(, $span:expr)?;
        $method:ident, $path:literal, $($arg:expr),*;
        $($hn:expr=>$hv:expr),*;
        form/multipart: $body:expr
    ) => {{
        let (span, target, url) = $crate::span!($client$(, $span)?; $method, $path, $($arg),*);
        $crate::req_form_multipart(&$client, span, target, $crate::Method::$method, &url, &[$(($hn, $hv)),*], $body)
    }};
    (
        $client:expr$(, $span:expr)?;
        $method:ident, $path:literal, $($arg:expr),*;
        $($hn:expr=>$hv:expr),*;
        form/urlencoded: $body:expr
    ) => {{
        let (span, target, url) = $crate::span!($client$(, $span)?; $method, $path, $($arg),*);
        $crate::req_form_urlencoded(&$client, span, target, $crate::Method::$method, &url, &[$(($hn, $hv)),*], $body)
    }};
}

#[macro_export]
macro_rules! span {
    (
        $client:expr;
        $method:ident, $target:literal, $($arg:expr),*
    ) => {{
        $crate::span!($client, &$crate::Context::current(); $method, $target,  $($arg),*)
    }};

    (
        $client:expr, $ctx:expr;
        $method:ident, $target:literal, $($arg:expr),*
    ) => {{
        use $crate::{Tracer, Span};
        let url = format!("{}{}", $client.uri, format!($target, $($arg),*));

        let name = concat!(stringify!($method), " ", $target);
        let mut span = $client.tracer.start_with_context(name, $ctx);

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
        use super::{client, req, Form};

        let client = client("test", hyper::Uri::from_static("/uri"));
        let ctx = opentelemetry::Context::current();

        // no body
        drop(req!(client; GET, "/oi/{}", "blz"; "auth" => "yo"));
        drop(req!(client, &ctx; GET, "/oi/{}", "blz"; "auth" => "yo"));

        // bare body
        drop(
            req!(client; GET, "/oi/{}", "blz"; "auth" => "yo"; Some("body".as_bytes().to_owned())),
        );
        drop(
            req!(client, &ctx; GET, "/oi/{}", "blz"; "auth" => "yo"; Some("body".as_bytes().to_owned())),
        );

        // json
        drop(req!(client; POST, "/oi/{}", "blz"; "auth" => "yo"; json: "serializable"));
        drop(req!(client, &ctx; POST, "/oi/{}", "blz"; "auth" => "yo"; json: "serializable"));

        // form multipart
        drop(req!(client; PUT, "/oi/{}", "blz"; "auth" => "yo"; form/multipart: Form::default()));
        drop(
            req!(client, &ctx; PUT, "/oi/{}", "blz"; "auth" => "yo"; form/multipart: Form::default()),
        );

        // form urlencoded
        drop(req!(client; PATCH, "/oi/{}", "blz"; "auth" => "yo"; form/urlencoded: ("oi", "blz")));
        drop(
            req!(client, &ctx; PATCH, "/oi/{}", "blz"; "auth" => "yo"; form/urlencoded: ("oi", "blz")),
        );
    }
}
