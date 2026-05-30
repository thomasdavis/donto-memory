//! Custom axum extractors with uniform error envelopes.
//!
//! Default `Json<T>` returns 422 + plain text on a deserialization
//! failure, which breaks the API's `{"error":"…"}` invariant. The
//! [`JsonReq<T>`] extractor preserves the typed-T ergonomics but
//! produces a 400 with the standard JSON shape.

use axum::async_trait;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde_json::json;

/// Drop-in replacement for `Json<T>` that converts deserialization
/// failures into our standard 400 `{"error":"…"}` envelope.
pub struct JsonReq<T>(pub T);

#[async_trait]
impl<S, T> FromRequest<S> for JsonReq<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = (StatusCode, axum::Json<serde_json::Value>);

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let result: Result<axum::Json<T>, JsonRejection> =
            axum::Json::<T>::from_request(req, state).await;
        match result {
            Ok(axum::Json(v)) => Ok(JsonReq(v)),
            Err(rej) => Err((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": rej.body_text()})),
            )),
        }
    }
}

impl<T: IntoResponse> IntoResponse for JsonReq<T> {
    fn into_response(self) -> Response {
        self.0.into_response()
    }
}
