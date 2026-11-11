use serde_json::{Value, json};
use worker::*;

#[durable_object]
pub struct UserHub {
    state: State,
}

impl DurableObject for UserHub {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        if req.method() == Method::Get && req.path() == "/open" {
            let user = req.headers().get("X-User-Id")?.unwrap_or_default();
            if !crate::security::uuid(&user) {
                return Ok(Response::error("invalid user", 403)?);
            }
            let pair = WebSocketPair::new()?;
            pair.server.serialize_attachment(&user)?;
            self.state.accept_web_socket(&pair.server);
            pair.server.send(&json!({"type":"realtime.ready"}))?;
            return Response::from_websocket(pair.client);
        }

        if req.method() != Method::Post {
            return Response::error("method not allowed", 405);
        }
        let value: Value = req.json().await?;
        let Some(target) = value["recipient_id"].as_str() else {
            return Response::error("recipient required", 422);
        };
        let Some(chat) = value["chat_id"].as_str() else {
            return Response::error("chat required", 422);
        };
        let Some(seq) = value["seq"].as_u64() else {
            return Response::error("sequence required", 422);
        };
        let notification = json!({"type":"chat.changed","chat_id":chat,"seq":seq});
        let mut connected = 0;
        for socket in self.state.get_websockets() {
            if socket.deserialize_attachment::<String>()?.as_deref() != Some(target) {
                continue;
            }
            if socket.send(&notification).is_ok() {
                connected += 1;
            }
        }
        // Accepted by the relay, not proof that a client/device received the event.
        Response::from_json(&json!({"relayed":true,"connected_sockets":connected}))
    }

    async fn websocket_message(&self, socket: WebSocket, msg: WebSocketIncomingMessage) -> Result<()> {
        match msg {
            WebSocketIncomingMessage::String(s) if s == "ping" => socket.send_with_str("pong"),
            _ => socket.close(Some(1003), Some("read-only notification socket")),
        }
    }

    async fn websocket_close(&self, _socket: WebSocket, _code: usize, _reason: String, _clean: bool) -> Result<()> {
        Ok(())
    }

    async fn websocket_error(&self, socket: WebSocket, _error: Error) -> Result<()> {
        socket.close(Some(1011), Some("websocket error"))
    }
}
