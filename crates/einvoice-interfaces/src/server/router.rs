//! The axum router: HTTP routing, memory admission, and body handling.
//!
//! [`router`] assembles the complete service — every route, the per-frame
//! body timeouts, and the `/transform` pipeline (reserve memory → read body
//! → transform on the blocking pool → respond). The binary only wires the
//! returned [`Router`] to a listener; everything decidable lives here and is
//! tested in-process with `tower::ServiceExt::oneshot`, no sockets.
//!
//! # Structure
//!
//! - [`router`] — builds the [`Router`] from a [`MemGate`], the reservation
//!   blowup factor, and the [`Timeouts`].
//! - `transform` — the `POST /transform` handler; the only route that reads
//!   a body and therefore the only one that touches the gate.
//! - `GuardedBody` — a response body that owns the request's memory
//!   reservation [`Guard`], releasing it only when the response has been
//!   written (or the connection died). Releasing on handler return instead
//!   would let a slow-reading client accumulate unreserved response XML.
//!
//! # Behavior
//!
//! Denial table (the transport half; `handle` owns the 400/422/500 rows):
//!
//! | Condition                            | Response                  |
//! |--------------------------------------|---------------------------|
//! | unknown route                        | 404 + usage text          |
//! | no Content-Length (chunked upload)   | 411 + `Connection: close` |
//! | reservation exceeds the whole budget | 413 + `Connection: close` |
//! | no reservation within queue timeout  | 503 + `Retry-After` + close |
//! | body not complete by request timeout | 408 + `Connection: close` |
//! | body read fails or a frame times out | 400 + `Connection: close` |
//! | body exceeds Content-Length          | 400 + `Connection: close` |
//!
//! The transformation itself is CPU-bound and runs via `spawn_blocking`; the
//! runtime caps that pool at the configured worker count, which bounds
//! transform parallelism the way the old thread-per-worker loop did.
//!
//! Bodies are read manually frame by frame (no extractor), so axum's default
//! 2 MB body limit never engages — there is deliberately no size limit; the
//! gate is the only admission policy.
//!
//! # Testing
//!
//! Unit tests drive the router with `oneshot` requests covering every row of
//! the denial table, the happy path, header placement, timeout firing (under
//! paused time), and reservation release timing.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{RawQuery, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use http_body::{Frame, SizeHint};
use http_body_util::BodyExt as _;
use tower_http::timeout::{RequestBodyTimeoutLayer, ResponseBodyTimeoutLayer};

use super::gate::{Guard, MemGate};
use super::handle::{self, Reply};

/// The server's time limits. Each bounds how long a client can hold a
/// connection — and, once admitted, its memory reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Longest gap between body frames, in both directions.
    pub body: Duration,
    /// Deadline for reading the whole request body. Without it a client
    /// sending one byte per `body` interval would hold its reservation
    /// indefinitely.
    pub request: Duration,
    /// Longest wait for a reservation before the request is shed with 503.
    pub queue: Duration,
}

/// Shared per-request context: the admission gate, the reservation
/// multiplier, and the time limits.
#[derive(Clone)]
struct AppState {
    gate: Arc<MemGate>,
    blowup: u64,
    timeouts: Timeouts,
}

/// The usage text served on unknown routes.
const USAGE: &str = "POST /transform?to=<format>[&from=<format>] | GET /formats | GET /analyze[?from=<format>[&to=<format>]][&deny_lossy=1] | GET /health";

/// Builds the complete `krab-server` service. `blowup` is the reservation
/// multiplier (`Content-Length x blowup` bytes are reserved per transform);
/// `timeouts` bound frame gaps, the whole upload, and the queue wait.
pub fn router(gate: Arc<MemGate>, blowup: u64, timeouts: Timeouts) -> Router {
    Router::new()
        .route("/health", get(async || "ok"))
        .route("/formats", get(formats))
        .route("/analyze", get(analyze))
        .route("/transform", post(transform))
        .fallback(async || (StatusCode::NOT_FOUND, USAGE))
        .layer(RequestBodyTimeoutLayer::new(timeouts.body))
        .layer(ResponseBodyTimeoutLayer::new(timeouts.body))
        .with_state(AppState {
            gate,
            blowup,
            timeouts,
        })
}

/// `GET /formats`: the JSON format list.
async fn formats() -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        handle::formats(),
    )
        .into_response()
}

/// `GET /analyze[?from=<format>[&to=<format>]][&deny_lossy=1]`: the
/// loss/error matrix, row or pair report as plain text; `deny_lossy` turns a
/// non-lossless result into a 422 carrying the report.
async fn analyze(RawQuery(query): RawQuery) -> Response {
    match handle::analyze(query.as_deref().unwrap_or("")) {
        Ok(table) => table.into_response(),
        Err((status, problem)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
            problem,
        )
            .into_response(),
    }
}

/// `POST /transform`: reserve memory, read the body, transform on the
/// blocking pool, respond with the reservation attached to the body.
async fn transform(State(state): State<AppState>, request: Request) -> Response {
    let query = request.uri().query().unwrap_or("").to_owned();

    // The reservation is sized from Content-Length, so a body without one
    // (chunked) cannot be admitted. Standard answer: 411 Length Required.
    // Hyper accepts chunked transparently — this check is what preserves the
    // contract.
    let Some(length) = content_length(&request) else {
        return plain(
            StatusCode::LENGTH_REQUIRED,
            "Content-Length required".into(),
        )
        .close();
    };

    // Reserve the request's estimated peak memory (body + parse blowup)
    // before reading a byte. Waits while the budget is exhausted — queued
    // requests here are the backpressure, bounded by the queue timeout so a
    // saturated server sheds load instead of parking connections whose
    // clients have long given up. Held until the response is written.
    let acquire = state
        .gate
        .clone()
        .acquire(length.saturating_mul(state.blowup));
    let reservation = match tokio::time::timeout(state.timeouts.queue, acquire).await {
        Ok(Ok(guard)) => guard,
        Ok(Err(never_fits)) => {
            return plain(StatusCode::PAYLOAD_TOO_LARGE, never_fits.to_string()).close();
        }
        Err(_elapsed) => {
            let mut response = plain(
                StatusCode::SERVICE_UNAVAILABLE,
                "server busy: no memory reservation within the queue timeout".into(),
            )
            .close();
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from(retry_after(state.timeouts)),
            );
            return response;
        }
    };

    // Read frame by frame into one pre-sized buffer: a single copy, and no
    // extractor means no default body limit. The timeout layer wraps this
    // body, so a stalled upload surfaces as a frame error, not a pinned task;
    // the request deadline bounds a trickling one.
    let read = read_body(request.into_body(), length);
    let body = match tokio::time::timeout(state.timeouts.request, read).await {
        Ok(Ok(body)) => body,
        Ok(Err(response)) => return response,
        Err(_elapsed) => {
            return plain(
                StatusCode::REQUEST_TIMEOUT,
                "request body not received within the request timeout".into(),
            )
            .close();
        }
    };

    // The transform is pure CPU for up to hundreds of ms: off the reactor.
    // The runtime caps the blocking pool at the worker count, bounding
    // transform parallelism. A panic is our bug: report 500, keep serving.
    let reply = match tokio::task::spawn_blocking(move || handle::handle(&query, body)).await {
        Ok(reply) => reply,
        Err(_) => {
            eprintln!("krab-server: recovered from a panic while transforming a request");
            Reply {
                status: 500,
                body: "internal error: transformation panicked".into(),
                warnings: String::new(),
            }
        }
    };
    respond(reply, reservation)
}

/// Reads exactly the body announced by `length` into one pre-sized buffer;
/// `Err` carries the response for a body that overruns `length` or fails.
async fn read_body(mut stream: Body, length: u64) -> Result<Vec<u8>, Response> {
    let mut body = Vec::with_capacity(length as usize);
    loop {
        match stream.frame().await {
            None => return Ok(body),
            Some(Ok(frame)) => {
                let Ok(data) = frame.into_data() else {
                    continue; // trailers etc. carry no body bytes
                };
                // Hyper enforces Content-Length framing on a real socket;
                // this keeps the reservation honest for any other transport.
                if body.len() + data.len() > length as usize {
                    return Err(plain(
                        StatusCode::BAD_REQUEST,
                        "request body exceeds Content-Length".into(),
                    )
                    .close());
                }
                body.extend_from_slice(&data);
            }
            Some(Err(e)) => {
                return Err(plain(
                    StatusCode::BAD_REQUEST,
                    format!("failed reading request body: {e}"),
                )
                .close());
            }
        }
    }
}

/// The `Retry-After` seconds for a request shed from the queue: one queue
/// timeout, at least one second.
fn retry_after(timeouts: Timeouts) -> u64 {
    timeouts.queue.as_secs().max(1)
}

/// Parses the `Content-Length` header; `None` when absent or unreadable
/// (hyper rejects malformed values before routing on a real connection).
fn content_length(request: &Request) -> Option<u64> {
    request
        .headers()
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Builds the `/transform` response carrying `reservation` in its body so
/// the bytes stay reserved until the response is actually written.
fn respond(reply: Reply, reservation: Guard) -> Response {
    let content_type = if reply.status == 200 {
        "application/xml"
    } else {
        "text/plain"
    };
    let mut builder = Response::builder()
        .status(reply.status)
        .header(header::CONTENT_TYPE, content_type)
        // Explicit: the response-timeout wrapper hides the body's exact size
        // hint from hyper, which would otherwise fall back to chunked.
        .header(header::CONTENT_LENGTH, reply.body.len());
    // Header values cannot hold arbitrary bytes; diagnostics are ASCII in
    // practice, and a warning header is not worth failing a 200 over.
    if !reply.warnings.is_empty()
        && let Ok(value) = HeaderValue::from_str(&reply.warnings)
    {
        builder = builder.header("X-Krab-Warnings", value);
    }
    builder
        .body(Body::new(GuardedBody {
            data: Some(reply.body.into()),
            _reservation: reservation,
        }))
        // Unreachable: status and headers above are validated. Never panic
        // in the serving path regardless.
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// A plain-text response. Chain [`Closing::close`] when the request body was
/// not (fully) read: the connection cannot be reused, and closing it stops
/// an in-flight upload at the TCP level.
fn plain(status: StatusCode, text: String) -> Response {
    let length = text.len();
    let mut response = (status, text).into_response();
    // Explicit for the same reason as in `respond`: the timeout wrapper
    // hides the exact size hint and hyper would send chunked.
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    response
}

trait Closing {
    /// Adds `Connection: close`.
    fn close(self) -> Response;
}

impl Closing for Response {
    fn close(mut self) -> Response {
        self.headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
        self
    }
}

/// A single-frame response body that owns the request's memory reservation.
///
/// The transformed XML is still resident while hyper streams it to the
/// client, so the reservation must live exactly as long as the body: it is
/// released when the body is dropped — after the last byte is written, or
/// when the response-side timeout kills a stalled connection.
struct GuardedBody {
    data: Option<Bytes>,
    _reservation: Guard,
}

impl http_body::Body for GuardedBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(self.data.take().map(|data| Ok(Frame::data(data))))
    }

    fn is_end_stream(&self) -> bool {
        self.data.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.data.as_ref().map_or(0, |d| d.len() as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt as _;

    const UBL: &[u8] = br#"<Invoice xmlns="urn:oasis:names:specification:ubl:schema:xsd:Invoice-2"><CustomizationID>urn:cen.eu:en16931:2017</CustomizationID>
        <ID>INV-42</ID>
        <IssueDate>2026-06-27</IssueDate><InvoiceTypeCode>380</InvoiceTypeCode>
        <DocumentCurrencyCode>EUR</DocumentCurrencyCode>
        <AccountingSupplierParty><Party><PostalAddress><Country><IdentificationCode>DE</IdentificationCode></Country></PostalAddress><PartyLegalEntity><RegistrationName>Seller GmbH</RegistrationName></PartyLegalEntity></Party></AccountingSupplierParty><AccountingCustomerParty><Party><PostalAddress><Country><IdentificationCode>DE</IdentificationCode></Country></PostalAddress><PartyLegalEntity><RegistrationName>Buyer AG</RegistrationName></PartyLegalEntity></Party></AccountingCustomerParty><TaxTotal><TaxAmount currencyID="EUR">19.00</TaxAmount><TaxSubtotal><TaxableAmount currencyID="EUR">100.00</TaxableAmount><TaxAmount currencyID="EUR">19.00</TaxAmount><TaxCategory><ID>S</ID><Percent>19</Percent><TaxScheme><ID>VAT</ID></TaxScheme></TaxCategory></TaxSubtotal></TaxTotal>
        <LegalMonetaryTotal>
            <LineExtensionAmount currencyID="EUR">100.00</LineExtensionAmount><TaxExclusiveAmount currencyID="EUR">100.00</TaxExclusiveAmount><TaxInclusiveAmount currencyID="EUR">119.00</TaxInclusiveAmount><PayableAmount currencyID="EUR">119.00</PayableAmount>
        </LegalMonetaryTotal>
        <InvoiceLine><ID>1</ID><InvoicedQuantity unitCode="C62">4</InvoicedQuantity><LineExtensionAmount currencyID="EUR">100.00</LineExtensionAmount><Item><Name>Widget</Name><ClassifiedTaxCategory><ID>S</ID><Percent>19</Percent><TaxScheme><ID>VAT</ID></TaxScheme></ClassifiedTaxCategory></Item><Price><PriceAmount currencyID="EUR">25.00</PriceAmount></Price></InvoiceLine>
    </Invoice>"#;

    const BIG_BUDGET: u64 = 64 * 1024 * 1024;

    const TIMEOUTS: Timeouts = Timeouts {
        body: Duration::from_secs(30),
        request: Duration::from_secs(300),
        queue: Duration::from_secs(60),
    };

    fn app(budget: u64) -> Router {
        router(Arc::new(MemGate::new(budget)), 7, TIMEOUTS)
    }

    /// A POST /transform request with an explicit Content-Length.
    fn transform_request(body: &'static [u8], query: &str) -> HttpRequest<Body> {
        HttpRequest::post(format!("/transform?{query}"))
            .header(header::CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .expect("valid request")
    }

    async fn body_text(response: Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body reads");
        String::from_utf8(bytes.to_vec()).expect("utf-8 body")
    }

    #[tokio::test]
    async fn test_health_returns_200_ok() {
        let response = app(BIG_BUDGET)
            .oneshot(HttpRequest::get("/health").body(Body::empty()).expect("ok"))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "ok");
    }

    #[tokio::test]
    async fn test_formats_returns_json_array() {
        let response = app(BIG_BUDGET)
            .oneshot(
                HttpRequest::get("/formats")
                    .body(Body::empty())
                    .expect("ok"),
            )
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let names: Vec<String> =
            serde_json::from_str(&body_text(response).await).expect("valid JSON");
        assert!(!names.is_empty());
    }

    #[tokio::test]
    async fn test_analyze_known_source_returns_table() {
        let response = app(BIG_BUDGET)
            .oneshot(
                HttpRequest::get("/analyze?from=ubl-invoice")
                    .body(Body::empty())
                    .expect("ok"),
            )
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(body_text(response).await.contains("legend:"));
    }

    #[tokio::test]
    async fn test_analyze_unknown_source_returns_400() {
        let response = app(BIG_BUDGET)
            .oneshot(
                HttpRequest::get("/analyze?from=not-a-format")
                    .body(Body::empty())
                    .expect("ok"),
            )
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_text(response).await.contains("not-a-format"));
    }

    #[tokio::test]
    async fn test_analyze_deny_lossy_returns_422_with_the_report() {
        let response = app(BIG_BUDGET)
            .oneshot(
                HttpRequest::get("/analyze?from=fatturapa&to=xrechnung-invoice&deny_lossy=1")
                    .body(Body::empty())
                    .expect("ok"),
            )
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let text = body_text(response).await;
        assert!(text.contains("BuyerName"), "{text}");
    }

    #[tokio::test]
    async fn test_unknown_route_returns_404_usage() {
        let response = app(BIG_BUDGET)
            .oneshot(HttpRequest::get("/nope").body(Body::empty()).expect("ok"))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(body_text(response).await.contains("/transform"));
    }

    #[tokio::test]
    async fn test_transform_valid_returns_200_xml() {
        let response = app(BIG_BUDGET)
            .oneshot(transform_request(UBL, "to=ubl-invoice&from=ubl-invoice"))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/xml");
        assert!(
            !response.headers().contains_key("X-Krab-Warnings"),
            "clean transform must not emit a warnings header"
        );
        let declared: usize = response.headers()[header::CONTENT_LENGTH]
            .to_str()
            .expect("ascii header")
            .parse()
            .expect("numeric length");
        let body = body_text(response).await;
        assert_eq!(
            declared,
            body.len(),
            "explicit Content-Length must match the body (the timeout \
             wrapper hides the size hint, so hyper cannot derive it)"
        );
        assert!(body.contains("<cbc:ID>INV-42</cbc:ID>"), "{body}");
    }

    #[tokio::test]
    async fn test_transform_problem_reply_is_text_plain() {
        let response = app(BIG_BUDGET)
            .oneshot(transform_request(UBL, "to=not-a-format"))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/plain");
        assert!(body_text(response).await.contains("not-a-format"));
    }

    #[tokio::test]
    async fn test_transform_without_content_length_returns_411_close() {
        let request = HttpRequest::post("/transform?to=ubl-invoice")
            .body(Body::from(UBL)) // no Content-Length header set
            .expect("valid request");
        let response = app(BIG_BUDGET).oneshot(request).await.expect("infallible");
        assert_eq!(response.status(), StatusCode::LENGTH_REQUIRED);
        assert_eq!(response.headers()[header::CONNECTION], "close");
    }

    #[tokio::test]
    async fn test_transform_reservation_over_budget_returns_413_close() {
        // budget 10 < len x 7: can never fit.
        let response = app(10)
            .oneshot(transform_request(UBL, "to=ubl-invoice"))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(response.headers()[header::CONNECTION], "close");
        assert!(body_text(response).await.contains("never fit"));
    }

    #[tokio::test]
    async fn test_transform_body_exceeding_content_length_returns_400_close() {
        let request = HttpRequest::post("/transform?to=ubl-invoice")
            .header(header::CONTENT_LENGTH, 10) // lies: body is larger
            .body(Body::from(UBL))
            .expect("valid request");
        let response = app(BIG_BUDGET).oneshot(request).await.expect("infallible");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[header::CONNECTION], "close");
        assert!(body_text(response).await.contains("Content-Length"));
    }

    /// A body that never yields: a client that sent headers and then went
    /// silent mid-upload.
    struct StalledBody;

    impl http_body::Body for StalledBody {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            Poll::Pending
        }
    }

    // Paused time: the runtime auto-advances the clock to the body-timeout
    // deadline instead of sleeping through it.
    #[tokio::test(start_paused = true)]
    async fn test_transform_stalled_upload_times_out_with_400_close() {
        let request = HttpRequest::post("/transform?to=ubl-invoice")
            .header(header::CONTENT_LENGTH, 1024)
            .body(Body::new(StalledBody))
            .expect("valid request");
        let response = app(BIG_BUDGET).oneshot(request).await.expect("infallible");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()[header::CONNECTION], "close");
        assert!(
            body_text(response)
                .await
                .contains("failed reading request body"),
            "timeout surfaces on the body-read path"
        );
    }

    #[tokio::test]
    async fn test_reservation_released_only_after_response_body_is_read() {
        let gate = Arc::new(MemGate::new(BIG_BUDGET));
        let app = router(gate.clone(), 7, TIMEOUTS);
        let response = app
            .oneshot(transform_request(UBL, "to=ubl-invoice&from=ubl-invoice"))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            gate.available(),
            BIG_BUDGET - (UBL.len() as u64) * 7,
            "reservation must still be held while the body is unread"
        );
        drop(response);
        assert_eq!(
            gate.available(),
            BIG_BUDGET,
            "dropping the response body must release the reservation"
        );
    }

    /// A body that yields one byte per `gap` forever: a client trickling its
    /// upload just inside the per-frame timeout.
    struct TricklingBody {
        gap: Duration,
        sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    }

    impl http_body::Body for TricklingBody {
        type Data = Bytes;
        type Error = Infallible;
        fn poll_frame(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
            let gap = self.gap;
            let sleep = self
                .sleep
                .get_or_insert_with(|| Box::pin(tokio::time::sleep(gap)));
            match sleep.as_mut().poll(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(()) => {
                    self.sleep = None;
                    Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"<")))))
                }
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_transform_trickling_upload_hits_request_timeout_with_408_close() {
        let gate = Arc::new(MemGate::new(BIG_BUDGET));
        let app = router(gate.clone(), 7, TIMEOUTS);
        let request = HttpRequest::post("/transform?to=ubl-invoice")
            .header(header::CONTENT_LENGTH, 1024 * 1024)
            .body(Body::new(TricklingBody {
                gap: Duration::from_secs(29), // inside the 30 s frame timeout
                sleep: None,
            }))
            .expect("valid request");
        let response = app.oneshot(request).await.expect("infallible");
        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
        assert_eq!(response.headers()[header::CONNECTION], "close");
        assert_eq!(
            gate.available(),
            BIG_BUDGET,
            "a timed-out upload must release its reservation"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_transform_queue_timeout_returns_503_retry_after_close() {
        let gate = Arc::new(MemGate::new(BIG_BUDGET));
        let held = gate
            .clone()
            .acquire(BIG_BUDGET)
            .await
            .expect("whole budget fits");
        let app = router(gate.clone(), 7, TIMEOUTS);
        let response = app
            .oneshot(transform_request(UBL, "to=ubl-invoice"))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::CONNECTION], "close");
        assert_eq!(response.headers()[header::RETRY_AFTER], "60");
        drop(held);
        assert_eq!(
            gate.available(),
            BIG_BUDGET,
            "the shed waiter reserved nothing"
        );
    }

    #[tokio::test]
    async fn test_transform_percent_encoded_target_returns_200() {
        let response = app(BIG_BUDGET)
            .oneshot(transform_request(UBL, "to=ubl-invoice%3A2.1"))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
    }
}
