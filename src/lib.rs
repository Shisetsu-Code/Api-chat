mod security;
mod room;
mod hub;

use serde_json::{Value, json};
use std::collections::BTreeSet;
use worker::*;
use worker::wasm_bindgen::JsValue;

fn json_response(v: &Value, code: u16) -> Result<Response> {
    Ok(Response::from_json(v)?.with_status(code))
}
fn error(code: u16, msg: &str) -> Result<Response> {
    json_response(&json!({"error":msg}), code)
}
fn bearer(req: &Request, env: &Env) -> Option<String> {
    security::verify(env, req.headers().get("Authorization").ok().flatten())
}
fn admin(req: &Request, env: &Env) -> bool {
    security::admin(env, req.headers().get("X-Admin-Key").ok().flatten())
}
async fn body(req: &mut Request) -> Option<Value> {
    let size = req.headers().get("Content-Length").ok().flatten()
        .and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    if size > 65536 { return None; }
    let text = req.text().await.ok()?;
    if text.len() > 65536 { return None; }
    serde_json::from_str(&text).ok()
}
async fn in_chat(env: &Env, chat: &str, user: &str) -> bool {
    let Ok(db) = env.d1("DB") else { return false };
    let Ok(stmt) = db.prepare("SELECT user_id FROM chat_members WHERE chat_id=?1 AND user_id=?2")
        .bind(&[JsValue::from_str(chat), JsValue::from_str(user)]) else { return false };
    stmt.first::<Value>(None).await.ok().flatten().is_some()
}
async fn members(env: &Env, chat: &str) -> Option<Vec<String>> {
    let db = env.d1("DB").ok()?;
    let stmt = db.prepare("SELECT user_id FROM chat_members WHERE chat_id=?1")
        .bind(&[JsValue::from_str(chat)]).ok()?;
    let rows = stmt.all().await.ok()?.results::<Value>().ok()?;
    Some(rows.into_iter().filter_map(|r| r["user_id"].as_str().map(str::to_string)).collect())
}
async fn internal(env: &Env, binding: &str, key: &str, op: &Value) -> Result<Response> {
    let stub = env.durable_object(binding)?.id_from_name(key)?.get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    init.with_body(Some(serde_json::to_string(op).map_err(|_| Error::RustError("serialization error".into()))?.into()));
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    init.with_headers(headers);
    stub.fetch_with_request(Request::new_with_init("https://internal/", &init)?).await
}

#[event(fetch)]
pub async fn main(mut req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.path();
    let method = req.method();
    if method == Method::Get && path == "/health" {
        return json_response(&json!({"status":"ok","runtime":"rust-wasm"}), 200);
    }
    if method == Method::Post && path == "/v1/admin/token" {
        if !admin(&req, &env) { return error(401, "unauthorized"); }
        let Some(data) = body(&mut req).await else { return error(400, "invalid body"); };
        let Some(user) = data["user_id"].as_str() else { return error(400, "user_id required"); };
        let ttl = data["ttl_seconds"].as_u64().unwrap_or(3600);
        return match security::issue(&env, user, ttl) {
            Ok(token) => json_response(&json!({"access_token": token, "expires_in": ttl, "token_type":"Bearer"}), 200),
            Err(_) => error(400, "invalid token request or key configuration")
        };
    }
    if method == Method::Post && path == "/v1/admin/chats" {
        if !admin(&req, &env) { return error(401, "unauthorized"); }
        let Some(data) = body(&mut req).await else { return error(400, "invalid body"); };
        let Some(chat) = data["chat_id"].as_str().filter(|v| security::uuid(v)) else { return error(400, "invalid chat_id"); };
        let Some(members) = data["members"].as_array() else { return error(400, "members required"); };
        let ids: BTreeSet<String> = members.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        if !(2..=100).contains(&ids.len()) || ids.len() != members.len() || !ids.iter().all(|s| security::uuid(s)) {
            return error(400, "require 2-100 unique uuid members");
        }
        let db = env.d1("DB")?;
        let mut statements = vec![db.prepare("INSERT INTO chats(chat_id) VALUES(?1)").bind(&[JsValue::from_str(chat)])?];
        for user in &ids {
            statements.push(db.prepare("INSERT INTO chat_members(chat_id,user_id) VALUES(?1,?2)")
                .bind(&[JsValue::from_str(chat), JsValue::from_str(user)])?);
        }
        return match db.batch(statements).await {
            Ok(_) => json_response(&json!({"chat_id": chat,"members": ids}), 201),
            Err(_) => error(409, "chat already exists or persistence failed")
        };
    }
    if method == Method::Get && path == "/v1/realtime" {
        let Some(user) = bearer(&req, &env) else { return error(401, "unauthorized"); };
        if req.headers().get("Upgrade")?.as_deref().map(str::to_lowercase).as_deref() != Some("websocket") {
            return error(426, "websocket required");
        }
        // Browser clients should use a short-lived cookie/ticket, not a token in a query string.
        let stub = env.durable_object("USER_HUB")?.id_from_name(&user)?.get_stub()?;
        let mut init = RequestInit::new();
        init.with_method(Method::Get);
        let headers = Headers::new();
        headers.set("Upgrade", "websocket")?;
        headers.set("X-User-Id", &user)?;
        init.with_headers(headers);
        return stub.fetch_with_request(Request::new_with_init("https://internal/open", &init)?).await;
    }
    let Some(user) = bearer(&req, &env) else { return error(401, "unauthorized"); };
    if method == Method::Get && path == "/v1/chats" {
        let db = env.d1("DB")?;
        let statement = db.prepare("SELECT chat_id FROM chat_members WHERE user_id=?1 ORDER BY chat_id LIMIT 1000")
            .bind(&[JsValue::from_str(&user)])?;
        return json_response(&json!({"chats":statement.all().await?.results::<Value>()?}), 200);
    }
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() != 5 || parts[1] != "v1" || parts[2] != "chats"
        || parts[4] != "messages" || !security::uuid(parts[3]) {
        return error(404, "not found");
    }
    let chat = parts[3].to_string();
    if !in_chat(&env, &chat, &user).await { return error(403, "not a chat member"); }
    if method == Method::Post {
        let allowed = env.rate_limiter("MESSAGE_RATE_LIMITER")?
            .limit(format!("message:{user}")).await?.success;
        if !allowed { return error(429, "message rate limit exceeded"); }
        let Some(data) = body(&mut req).await else { return error(400, "invalid body"); };
        let Some(id) = data["client_message_id"].as_str().filter(|v| security::uuid(v)) else {
            return error(400, "invalid client_message_id");
        };
        let Some(text) = data["text"].as_str().filter(|s| !s.is_empty() && s.len() <= 16384) else {
            return error(400, "message must contain 1-16384 bytes");
        };
        let Some(recipients) = members(&env, &chat).await else { return error(503, "members unavailable"); };
        return internal(&env, "CHAT_ROOM", &chat,
            &json!({"op":"send","chat_id":chat,"sender_id":user,"client_message_id":id,"text":text,"recipients":recipients})).await;
    }
    if method == Method::Get {
        let url = req.url()?;
        let after = url.query_pairs().find(|(k,_)| k == "after")
            .and_then(|(_,v)| v.parse::<u64>().ok()).unwrap_or(0);
        let limit = url.query_pairs().find(|(k,_)| k == "limit")
            .and_then(|(_,v)| v.parse::<u64>().ok()).unwrap_or(100);
        if !(1..=200).contains(&limit) || after > 9_007_199_254_740_991 {
            return error(400, "invalid pagination");
        }
        return internal(&env, "CHAT_ROOM", &chat,
            &json!({"op":"sync","chat_id":chat,"after":after,"limit":limit})).await;
    }
    error(405, "method not allowed")
}
