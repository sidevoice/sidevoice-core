//! Bounded paid provider tries, using the shared catalogue verdicts and SDK adapters.

use super::*;
use crate::messages::{render, LocalizedMessage};
use axum::extract::{Extension, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

mod budget;
mod provider;
#[cfg(test)]
mod tests;

pub(super) use budget::CheckBudget;

use budget::{check_key, Admission};
use provider::{check_provider, provider_label, refusal};

fn invalid_stage(language: &str) -> Value {
    let details = render(&LocalizedMessage::new("settings.stage_invalid"), language);
    refusal(
        LocalizedMessage::new("check_invalid").with_param("details", details),
        language,
    )
}

fn detail(status: StatusCode, reason: Value) -> Response {
    (status, Json(json!({"detail":reason}))).into_response()
}

/// `base` with every field of a check result added to it.
fn with_fields(mut base: Value, result: &Value) -> Value {
    if let Some(fields) = result.as_object() {
        for (name, field) in fields {
            base[name] = field.clone();
        }
    }
    base
}

/// The refusal for a request a provider check cannot serve at all.
fn unservable(task: &str, place: &str, ui: &str) -> Option<Response> {
    if !matches!(task, "stt" | "tts") {
        return Some(detail(StatusCode::UNPROCESSABLE_ENTITY, invalid_stage(ui)));
    }
    if place == "host" {
        return Some(detail(
            StatusCode::CONFLICT,
            refusal(LocalizedMessage::new("place_host_unavailable"), ui),
        ));
    }
    if place == "device" {
        return Some(detail(
            StatusCode::BAD_REQUEST,
            refusal(LocalizedMessage::new("check_on_device"), ui),
        ));
    }
    None
}

fn rate_limited(wait: u64, scope: &str, ui: &str) -> Response {
    let reason = refusal(
        LocalizedMessage::new("check_rate_limited")
            .with_param("retry_after", wait)
            .with_param("scope", scope),
        ui,
    );
    let mut response = detail(StatusCode::TOO_MANY_REQUESTS, reason);
    if let Ok(value) = HeaderValue::from_str(&wait.to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

pub(super) async fn model_check(
    State(state): State<Arc<AppState>>,
    Extension(device): Extension<AuthenticatedDevice>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return failure("request.origin_invalid", StatusCode::FORBIDDEN, &headers);
    }
    let Some(data) = payload(&body) else {
        return failure("room.request_invalid", StatusCode::BAD_REQUEST, &headers);
    };
    let ui = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("en")
        .to_owned();
    let task = data["stage"].as_str().unwrap_or("");
    let place = data["place"].as_str().unwrap_or("");
    if let Some(response) = unservable(task, place, &ui) {
        return response;
    }
    let stage_input = json!({"place":place,"model":data["model"],"options":data.get("options").filter(|value|value.is_object()).cloned().unwrap_or_else(||json!({}))});
    let Some(stage) = crate::models::provider_check_stage(task, &stage_input) else {
        return detail(StatusCode::UNPROCESSABLE_ENTITY, invalid_stage(&ui));
    };
    let language = data["language"].as_str().map(str::to_owned);
    let Some(key) = media::provider_key(&state.dir, &stage.place) else {
        return Json(json!({"stage":task,"place":stage.place,"model":stage.model,
            "ok":false,"step":"key","reason":refusal(LocalizedMessage::new("provider_key_missing")
                .with_param("provider",stage.place.clone()).with_param("provider_label",provider_label(&stage.place)),&ui),"passes":[]})).into_response();
    };
    let budget_key = check_key(task, &stage, language.as_deref(), &key);
    let admission = state
        .check_budget
        .admit(&budget_key, &device.0, &stage.place);
    let mut receiver = match admission {
        Admission::Cached(value) => {
            let base = json!({"stage":task,"place":stage.place,"model":stage.model});
            return Json(with_fields(base, &value)).into_response();
        }
        Admission::Limited(wait, scope) => return rate_limited(wait, scope, &ui),
        Admission::Join(receiver) => receiver,
        Admission::Start(receiver) => {
            let budget = state.clone();
            let key_for_task = budget_key.clone();
            let task_for_work = task.to_owned();
            let ui_for_work = ui.clone();
            tokio::spawn(async move {
                let answer =
                    check_provider(&task_for_work, stage, language, key, ui_for_work).await;
                budget.check_budget.complete(&key_for_task, answer);
            });
            receiver
        }
    };
    if receiver.changed().await.is_err() {
        return failure("start.failed", StatusCode::INTERNAL_SERVER_ERROR, &headers);
    }
    let Some(result) = receiver.borrow().clone() else {
        return failure("start.failed", StatusCode::INTERNAL_SERVER_ERROR, &headers);
    };
    let base = json!({"stage":task,"place":place,"model":data["model"]});
    Json(with_fields(base, &result)).into_response()
}
