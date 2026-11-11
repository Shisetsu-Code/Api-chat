//! Public HTTP API. Conversation Durable Objects are the only chat authorization authority.
//! D1 sessions are checked for every operation; D1 chat lists are read-only projections.
mod security;
mod db;
mod room;
mod room_store;
mod room_crypto;
mod hub;

use db::Db;
use serde_json::{json, Value};
use worker::*;

const MAX_BODY: usize = 65_536;
const MAX_TEXT: usize = 16_384;

fn reply(value: &Value, status: u16) -> Result<Response> {
    let mut res = Response::from_json(value)?.with_status(status);
    res.headers_mut().set("Cache-Control", "no-store")?;
    res.headers_mut().set("X-Content-Type-Options", "nosniff")?;
    res.headers_mut().set("Referrer-Policy", "no-referrer")?;
    Ok(res)
}
fn err(status: u16, code: &str) -> Result<Response> {
    reply(&json!({"error":code}), status)
}
fn internal_error() -> Error { Error::RustError("internal serialization error".into()) }
fn json_bytes(value: &Value) -> Result<String> {
    serde_json::to_string(value).map_err(|_| internal_error())
}

async fn body(req: &mut Request) -> Option<Value> {
    if req.headers().get("Content-Length").ok().flatten()
        .and_then(|n| n.parse::<usize>().ok()).is_some_and(|n| n > MAX_BODY) {
        return None;
    }
    let text = req.text().await.ok()?;
    if text.len() > MAX_BODY { return None; }
    serde_json::from_str(&text).ok()
}

fn admin(req: &Request, env: &Env) -> bool {
    security::admin(env, req.headers().get("X-Admin-Key").ok().flatten())
}

fn origin_allowed(req: &Request, env: &Env, required: bool) -> bool {
    let origin = req.headers().get("Origin").ok().flatten();
    let expected = env.var("PUBLIC_ORIGIN").ok().map(|x| x.to_string());
    match (origin, expected) {
        (Some(value), Some(expected)) => value == expected,
        (None, _) => !required,
        _ => false,
    }
}

async fn active_session(env: &Env, claims: &security::Claims) -> bool {
    let Ok(db) = Db::from_env(env) else { return false };
    let row = db.first(
        "SELECT sid FROM auth_sessions WHERE sid=?1 AND user_id=?2 AND revoked_ms IS NULL AND expires_ms>?3",
        &[json!(claims.sid), json!(claims.sub), json!(security::now_ms())],
    ).await;
    row.ok().flatten().is_some()
}

async fn access(req: &Request, env: &Env) -> Option<security::Claims> {
    let token = security::bearer(req.headers().get("Authorization").ok().flatten())?;
    let claims = security::verify(env, &token, "access")?;
    active_session(env, &claims).await.then_some(claims)
}

async fn call(env: &Env, binding: &str, id: &str, payload: &Value) -> Result<Response> {
    let stub = env.durable_object(binding)?.id_from_name(id)?.get_stub()?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post);
    init.with_body(Some(json_bytes(payload)?.into()));
    let headers = Headers::new();
    headers.set("Content-Type", "application/json")?;
    init.with_headers(headers);
    stub.fetch_with_request(Request::new_with_init("https://internal/action", &init)?).await
}

fn path_segments(path: &str) -> Vec<&str> {
    path.trim_matches('/').split('/').collect()
}
fn positive(value: &Value) -> Option<u64> {
    value.as_u64().filter(|n| *n <= 9_007_199_254_740_991)
}

#[event(fetch)]
pub async fn main(mut request: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = request.path();
    let method = request.method();
    if method == Method::Get && path == "/health" {
        return reply(&json!({"status":"ok","runtime":"rust-wasm","api_version":"v1"}), 200);
    }

    // Admin API is intended for server-to-server provisioning behind Cloudflare Access.
    if path.starts_with("/v1/admin/") {
        if !admin(&request, &env) { return err(401, "unauthorized"); }
        let db = Db::from_env(&env)?;
        if method == Method::Post && path == "/v1/admin/token" {
            let Some(data) = body(&mut request).await else { return err(400, "invalid_json"); };
            let Some(user) = data["user_id"].as_str().filter(|v| security::uuid(v)) else {
                return err(400,"invalid_user_id");
            };
            let ttl = data["ttl_seconds"].as_u64().unwrap_or(3600);
            if !(1..=3600).contains(&ttl) { return err(400,"invalid_ttl"); }
            let sid = security::random_id().map_err(Error::RustError)?;
            let token = security::issue(&env, user, &sid, ttl, "access").map_err(Error::RustError)?;
            db.execute("INSERT INTO auth_sessions(sid,user_id,created_ms,expires_ms) VALUES(?1,?2,?3,?4)",
                &[json!(sid),json!(user),json!(security::now_ms()),json!((security::now()+ttl)*1000)]).await?;
            return reply(&json!({"access_token":token,"token_type":"Bearer","expires_in":ttl,"session_id":sid}),201);
        }
        if method == Method::Post && path == "/v1/admin/chats" {
            let Some(data) = body(&mut request).await else {return err(400,"invalid_json");};
            let Some(chat) = data["chat_id"].as_str().filter(|v| security::uuid(v)) else {
                return err(400,"invalid_chat_id");
            };
            let Some(members) = data["members"].as_array() else {return err(400,"members_required");};
            let members: Vec<String> = members.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
            let unique: std::collections::BTreeSet<_> = members.iter().collect();
            if members.len() < 2 || members.len()>100 || unique.len()!=members.len()
                || !members.iter().all(|v| security::uuid(v)) {
                return err(400,"invalid_members");
            }
            return call(&env,"CHAT_ROOM",chat,&json!({"op":"create","chat_id":chat,"members":members})).await;
        }
        let p=path_segments(&path);
        if p.len()==6 && p[0]=="v1" && p[1]=="admin" && p[2]=="chats"
            && p[4]=="members" && security::uuid(p[3]) && security::uuid(p[5]) {
            let op = match method {
                Method::Put => "member.add",
                Method::Delete => "member.remove",
                _ => return err(405,"method_not_allowed"),
            };
            return call(&env,"CHAT_ROOM",p[3],
                &json!({"op":op,"chat_id":p[3],"member_id":p[5]})).await;
        }
        if p.len()==5 && p[0]=="v1" && p[1]=="admin" && p[2]=="chats"
            && security::uuid(p[3]) && method==Method::Post {
            match p[4] {
                "rotate" => {
                    let Some(data)=body(&mut request).await else {return err(400,"invalid_json");};
                    let Some(target)=data["target_key_version"].as_u64().filter(|n| (1..=2).contains(n))
                        else {return err(400,"invalid_key_version");};
                    return call(&env,"CHAT_ROOM",p[3],
                        &json!({"op":"rotate","chat_id":p[3],"target_key_version":target})).await;
                },
                "prune" => {
                    let Some(data)=body(&mut request).await else {return err(400,"invalid_json");};
                    let Some(before)=data["before_ms"].as_i64().filter(|t| *t>0 && *t<security::now_ms())
                        else {return err(400,"invalid_before_ms");};
                    return call(&env,"CHAT_ROOM",p[3],
                        &json!({"op":"prune","chat_id":p[3],"before_ms":before})).await;
                },
                _ => {}
            }
        }
        return err(404,"not_found");
    }

    if method==Method::Get && path=="/v1/realtime" {
        if !origin_allowed(&request,&env,false) {return err(403,"invalid_origin");}
        let authorization = security::bearer(request.headers().get("Authorization")?);
        let claims = if let Some(token)=authorization {
            security::verify(&env,&token,"access")
        } else {
            let ticket=security::cookie(request.headers().get("Cookie")?,"__Host-api-chat-realtime");
            ticket.and_then(|token| security::verify(&env,&token,"ws"))
        };
        let Some(claims)=claims else {return err(401,"unauthorized");};
        if !active_session(&env,&claims).await {return err(401,"session_revoked");}
        if request.headers().get("Upgrade")?.as_deref().map(str::to_lowercase).as_deref()
            != Some("websocket") {return err(426,"upgrade_required");}
        let hub=env.durable_object("USER_HUB")?.id_from_name(&claims.sub)?.get_stub()?;
        let mut init=RequestInit::new();
        init.with_method(Method::Get);
        let headers=Headers::new();
        headers.set("Upgrade","websocket")?;
        headers.set("X-Auth-Context",&json_bytes(&json!({
            "user_id":claims.sub,"session_id":claims.sid,"expires_at":claims.exp
        }))?)?;
        init.with_headers(headers);
        return hub.fetch_with_request(Request::new_with_init("https://internal/open",&init)?).await;
    }

    let Some(identity)=access(&request,&env).await else {return err(401,"unauthorized");};

    if method==Method::Post && path=="/v1/realtime/browser-session" {
        if !origin_allowed(&request,&env,true) {return err(403,"invalid_origin");}
        let token=security::issue(&env,&identity.sub,&identity.sid,120,"ws").map_err(Error::RustError)?;
        let mut response=reply(&json!({"websocket_path":"/v1/realtime","expires_in":120}),200)?;
        response.headers_mut().set("Set-Cookie",
            &format!("__Host-api-chat-realtime={token}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=120"))?;
        return Ok(response);
    }
    if method==Method::Post && path=="/v1/auth/refresh" {
        let db=Db::from_env(&env)?;
        let Some(row)=db.first("SELECT expires_ms FROM auth_sessions WHERE sid=?1 AND user_id=?2 AND revoked_ms IS NULL",
            &[json!(identity.sid),json!(identity.sub)]).await? else {return err(401,"session_revoked");};
        let remaining=(row["expires_ms"].as_i64().unwrap_or(0)-security::now_ms()).max(0)/1000;
        if remaining==0 {return err(401,"session_expired");}
        let ttl=(remaining as u64).min(3600);
        let token=security::issue(&env,&identity.sub,&identity.sid,ttl,"access").map_err(Error::RustError)?;
        return reply(&json!({"access_token":token,"expires_in":ttl,"token_type":"Bearer"}),200);
    }
    if method==Method::Post && path=="/v1/auth/revoke" {
        let db=Db::from_env(&env)?;
        db.execute("UPDATE auth_sessions SET revoked_ms=?3 WHERE sid=?1 AND user_id=?2 AND revoked_ms IS NULL",
            &[json!(identity.sid),json!(identity.sub),json!(security::now_ms())]).await?;
        // Revocation has been committed. Hub also checks D1 on every event,
        // so failed immediate disconnect cannot expose notifications.
        let _=call(&env,"USER_HUB",&identity.sub,
            &json!({"op":"disconnect","session_id":identity.sid})).await;
        return reply(&json!({"revoked":true}),200);
    }
    if method==Method::Get && path=="/v1/chats" {
        let db=Db::from_env(&env)?;
        let chats=db.all(
            "SELECT chat_id FROM chat_member_index WHERE user_id=?1 AND active=1 ORDER BY chat_id LIMIT 1000",
            &[json!(identity.sub)]
        ).await?;
        return reply(&json!({"chats":chats,"projection":"eventual"}),200);
    }

    let p=path_segments(&path);
    if p.len()!=4 || p[0]!="v1" || p[1]!="chats" || !security::uuid(p[2]) {
        return err(404,"not_found");
    }
    let chat=p[2];
    match (method,p[3]) {
        (Method::Post,"messages") => {
            let allowed=env.rate_limiter("MESSAGE_RATE_LIMITER")?
                .limit(format!("user:{}",identity.sub)).await?.success;
            if !allowed {return err(429,"rate_limited");}
            let Some(data)=body(&mut request).await else {return err(400,"invalid_json");};
            let Some(id)=data["client_message_id"].as_str().filter(|s|security::uuid(s))
                else {return err(400,"invalid_message_id");};
            let Some(text)=data["text"].as_str().filter(|s|!s.is_empty()&&s.len()<=MAX_TEXT)
                else {return err(400,"invalid_text");};
            call(&env,"CHAT_ROOM",chat,
                &json!({"op":"send","chat_id":chat,"user_id":identity.sub,"client_id":id,"text":text})).await
        }
        (Method::Get,"messages") => {
            let url=request.url()?;
            let after=url.query_pairs().find(|(k,_)|k=="after")
                .and_then(|(_,v)|v.parse::<u64>().ok()).unwrap_or(0);
            let limit=url.query_pairs().find(|(k,_)|k=="limit")
                .and_then(|(_,v)|v.parse::<u64>().ok()).unwrap_or(100);
            if limit==0 || limit>200 || after>9_007_199_254_740_991 {return err(400,"invalid_pagination");}
            call(&env,"CHAT_ROOM",chat,
                &json!({"op":"sync","chat_id":chat,"user_id":identity.sub,"after":after,"limit":limit})).await
        }
        (Method::Post,"receipts") => {
            let Some(data)=body(&mut request).await else {return err(400,"invalid_json");};
            let delivered=positive(&data["delivered_seq"]);
            let read=positive(&data["read_seq"]);
            if delivered.is_none()&&read.is_none(){return err(400,"cursor_required");}
            call(&env,"CHAT_ROOM",chat,&json!({
                "op":"receipt","chat_id":chat,"user_id":identity.sub,
                "delivered_seq":delivered,"read_seq":read
            })).await
        }
        (Method::Get,"receipts") => {
            call(&env,"CHAT_ROOM",chat,
                &json!({"op":"receipts.get","chat_id":chat,"user_id":identity.sub})).await
        }
        _ => err(405,"method_not_allowed")
    }
}
