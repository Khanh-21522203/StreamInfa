use axum::http::{header::HeaderName, HeaderValue, Request, Response};
use std::task::{Context, Poll};
use tower::{Layer, Service};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// X-Request-Id middleware (from observability.md §9.1)
// ---------------------------------------------------------------------------

/// Header name for request ID propagation.
pub static X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// Layer that adds `X-Request-Id` to every request and response.
///
/// From observability.md §9.1:
/// - If the incoming request has an `X-Request-Id` header, it is reused.
/// - Otherwise, a new UUIDv4 is generated.
/// - The request ID is set on the response header.
#[derive(Clone)]
pub struct RequestIdLayer;

impl<S> Layer<S> for RequestIdLayer {
    type Service = RequestIdMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestIdMiddleware { inner }
    }
}

/// Middleware service that injects `X-Request-Id`.
#[derive(Clone)]
pub struct RequestIdMiddleware<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for RequestIdMiddleware<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        // Extract or generate request ID
        let request_id = req
            .headers()
            .get(&X_REQUEST_ID)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        // Insert request ID into request headers (for downstream use)
        if let Ok(val) = HeaderValue::from_str(&request_id) {
            req.headers_mut().insert(X_REQUEST_ID.clone(), val);
        }

        let mut inner = self.inner.clone();
        let rid = request_id;

        Box::pin(async move {
            let mut response = inner.call(req).await?;

            // Set request ID on response
            if let Ok(val) = HeaderValue::from_str(&rid) {
                response.headers_mut().insert(X_REQUEST_ID.clone(), val);
            }

            Ok(response)
        })
    }
}
