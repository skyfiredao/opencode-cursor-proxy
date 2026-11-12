use crate::{agent::Update, error::ApiError};
use axum::{
    Json,
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn new_id() -> String {
    format!(
        "chatcmpl-{}-{}-{}",
        unix_time(),
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn chunk(id: &str, model: &str, created: u64, delta: Value, finish: Option<&str>) -> Value {
    json!({"id":id, "object":"chat.completion.chunk", "created":created, "model":model,
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
}

async fn send(tx: &mpsc::Sender<Result<Event, Infallible>>, body: Value) -> bool {
    tx.send(Ok(Event::default().data(body.to_string())))
        .await
        .is_ok()
}

pub async fn respond(
    mut rx: mpsc::Receiver<Update>,
    streaming: bool,
    model: String,
    id: String,
) -> Result<Response, ApiError> {
    let first = rx
        .recv()
        .await
        .ok_or_else(|| ApiError::upstream("cursor-agent produced no response"))?;
    if let Update::Error(error) = &first {
        return Err(error.clone());
    }
    let created = unix_time();
    if !streaming {
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls = Vec::new();
        let mut next = Some(first);
        loop {
            match next {
                Some(Update::Text(delta)) => text.push_str(&delta),
                Some(Update::Thinking(delta)) => reasoning.push_str(&delta),
                Some(Update::Tools(tool_calls)) => calls.extend(tool_calls),
                Some(Update::Error(error)) => return Err(error),
                Some(Update::Done(usage)) => {
                    let finish = if calls.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    };
                    let mut message = json!({"role":"assistant", "content":text});
                    if !reasoning.is_empty() {
                        message["reasoning_content"] = json!(reasoning);
                    }
                    if !calls.is_empty() {
                        if text.is_empty() {
                            message["content"] = Value::Null;
                        }
                        message["tool_calls"] = json!(calls);
                    }
                    let mut body = json!({"id":id,"object":"chat.completion","created":created,"model":model,
                        "choices":[{"index":0,"message":message,"finish_reason":finish}]});
                    if let Some(usage) = usage {
                        body["usage"] = usage.openai();
                    }
                    return Ok(Json(body).into_response());
                }
                None => {
                    return Err(ApiError::upstream(
                        "cursor-agent response ended without a final result",
                    ));
                }
            }
            next = rx.recv().await;
        }
    }

    let (tx, output) = mpsc::channel(16);
    tokio::spawn(async move {
        if !send(
            &tx,
            chunk(&id, &model, created, json!({"role":"assistant"}), None),
        )
        .await
        {
            return;
        }
        let mut next = Some(first);
        let mut has_tools = false;
        loop {
            let body = match next {
                Some(Update::Text(delta)) => {
                    chunk(&id, &model, created, json!({"content":delta}), None)
                }
                Some(Update::Thinking(delta)) => chunk(
                    &id,
                    &model,
                    created,
                    json!({"reasoning_content":delta}),
                    None,
                ),
                Some(Update::Tools(calls)) => {
                    has_tools = true;
                    let deltas: Vec<Value> = calls.into_iter().enumerate().map(|(index, call)| {
                        json!({"index":index,"id":call.id,"type":"function","function":call.function})
                    }).collect();
                    chunk(&id, &model, created, json!({"tool_calls":deltas}), None)
                }
                Some(Update::Done(usage)) => {
                    let finish = if has_tools { "tool_calls" } else { "stop" };
                    if !send(&tx, chunk(&id, &model, created, json!({}), Some(finish))).await {
                        return;
                    }
                    if let Some(usage) = usage
                        && !send(
                            &tx,
                            json!({"id":id,"object":"chat.completion.chunk","created":created,
                            "model":model,"choices":[],"usage":usage.openai()}),
                        )
                        .await
                    {
                        return;
                    }
                    let _delivered = tx.send(Ok(Event::default().data("[DONE]"))).await;
                    return;
                }
                Some(Update::Error(error)) => {
                    if send(&tx, error.body()).await {
                        let _delivered = tx.send(Ok(Event::default().data("[DONE]"))).await;
                    }
                    return;
                }
                None => {
                    let error =
                        ApiError::upstream("cursor-agent stream ended without a final result");
                    let _delivered = send(&tx, error.body()).await;
                    return;
                }
            };
            if !send(&tx, body).await {
                return;
            }
            next = tokio::select! {
                _ = tx.closed() => return,
                update = rx.recv() => update,
            };
        }
    });
    Ok(Sse::new(ReceiverStream::new(output))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response())
}
