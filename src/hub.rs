//! User-scoped WebSocket relay. It is NOT a message database.
//! One hibernating WebSocket per device (or browser tab); events are hints to sync.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use worker::*;
use crate::db::Db;

#[derive(Clone, Serialize, Deserialize)]
struct SocketAuth {
    user_id: String,
    session_id: String,
    expires_at: u64,
}

#[durable_object]
pub struct UserHub {
    state: State,
    env: Env,
}

impl UserHub {
    async fn live(&self, auth: &SocketAuth) -> bool {
        if auth.expires_at <= crate::security::now() { return false; }
        let Ok(db) = Db::from_env(&self.env) else { return false; };
        let result=db.first(
            "SELECT sid FROM auth_sessions WHERE sid=?1 AND user_id=?2 AND revoked_ms IS NULL AND expires_ms>?3",
            &[json!(auth.session_id),json!(auth.user_id),json!(crate::security::now_ms())]
        ).await;
        result.ok().flatten().is_some()
    }
    fn belongs(&self, user: &str) -> Result<bool> {
        Ok(crate::security::uuid(user) &&
            self.state.id().to_string()==self.env.durable_object("USER_HUB")?
                .id_from_name(user)?.to_string())
    }
    async fn notify(&self, v: &Value) -> Result<Response> {
        let user=v["recipient_id"].as_str().unwrap_or_default();
        let chat=v["chat_id"].as_str().unwrap_or_default();
        let kind=v["kind"].as_str().unwrap_or_default();
        let seq=v["seq"].as_u64().unwrap_or(0);
        if !self.belongs(user)? || !crate::security::uuid(chat) || seq==0 ||
            !matches!(kind,"message"|"receipt"|"membership") {
            return Response::error("invalid relay", 422);
        }
        let event=json!({
            "type":match kind {"message"=>"chat.changed","receipt"=>"receipt.changed",_=>"membership.changed"},
            "chat_id":chat, "seq":seq,
            "event_id":format!("{chat}/{kind}/{seq}/{user}")
        });
        let mut forwarded=0;
        // Fail closed if D1 is unavailable. This also enforces immediate
        // revocation even if the disconnect RPC failed after D1 committed.
        for socket in self.state.get_websockets() {
            let Some(auth)=socket.deserialize_attachment::<SocketAuth>()? else {
                let _=socket.close(Some(4001),Some("Missing session"));
                continue;
            };
            if auth.user_id!=user || !self.live(&auth).await {
                let _=socket.close(Some(4001),Some("Session revoked or expired"));
                continue;
            }
            if socket.send(&event).is_ok() { forwarded+=1; }
        }
        // Acknowledges delivery to UserHub only, never to the recipient's device.
        Response::from_json(&json!({"relayed":true,"connected_sockets":forwarded}))
    }
}

impl DurableObject for UserHub {
    fn new(state: State, env: Env) -> Self { Self { state, env } }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        if req.method()==Method::Get && req.path()=="/open" {
            let context=req.headers().get("X-Auth-Context")?.unwrap_or_default();
            let auth:SocketAuth=match serde_json::from_str(&context) {
                Ok(v)=>v,Err(_)=>return Response::error("invalid identity",403),
            };
            if !self.belongs(&auth.user_id)? || !self.live(&auth).await {
                return Response::error("session invalid",401);
            }
            if self.state.get_websockets().len()>=64 {
                return Response::error("too many sockets for user",429);
            }
            let pair=WebSocketPair::new()?;
            pair.server.serialize_attachment(&auth)?;
            self.state.accept_web_socket(&pair.server);
            pair.server.send(&json!({"type":"realtime.ready","user_id":auth.user_id}))?;
            return Response::from_websocket(pair.client);
        }
        if req.method()!=Method::Post {return Response::error("method not allowed",405);}
        let v:Value=req.json().await?;
        match v["op"].as_str() {
            Some("notify")=>self.notify(&v).await,
            Some("disconnect")=>{
                let sid=v["session_id"].as_str().unwrap_or_default();
                if !crate::security::uuid(sid) {return Response::error("bad session",422);}
                let mut attempted=0;
                let mut signaled=0;
                let mut closed=0;
                for socket in self.state.get_websockets() {
                    if let Some(auth)=socket.deserialize_attachment::<SocketAuth>()?
                        && auth.session_id==sid {
                        attempted+=1;
                        if socket.send(&json!({"type":"session.revoked","code":4001})).is_ok() {
                            signaled+=1;
                        }
                        match socket.close(Some(4001),Some("Session revoked")) {
                            Ok(())=>closed+=1,
                            Err(error)=>console_error!("UserHub: closing revoked websocket failed: {:?}",error),
                        }
                    }
                }
                Response::from_json(&json!({"closed":closed,"attempted":attempted,"signaled":signaled}))
            },
            _=>Response::error("unknown operation",404),
        }
    }

    async fn websocket_message(&self, socket: WebSocket, msg: WebSocketIncomingMessage) -> Result<()> {
        let Some(auth)=socket.deserialize_attachment::<SocketAuth>()? else {
            return socket.close(Some(4001),Some("Missing session"));
        };
        if !self.live(&auth).await {
            return socket.close(Some(4001),Some("Session expired"));
        }
        match msg {
            WebSocketIncomingMessage::String(s) if s=="ping" =>
                socket.send_with_str("pong"),
            _=>socket.close(Some(1003),Some("Read-only notification socket")),
        }
    }
    async fn websocket_close(&self, _socket: WebSocket,_code:usize,_reason:String,_clean:bool)->Result<()> {
        Ok(())
    }
    async fn websocket_error(&self,socket:WebSocket,_error:Error)->Result<()> {
        socket.close(Some(1011),Some("WebSocket error"))
    }
}
