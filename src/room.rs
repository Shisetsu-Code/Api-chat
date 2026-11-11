use aes_gcm::{Aes256Gcm, Nonce, aead::{Aead, KeyInit, Payload}};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::Sha256;
use hmac::{Hmac, Mac};
use std::{cell::RefCell, rc::Rc};
use worker::{SqlStorageValue as S, *};
use worker::wasm_bindgen::{JsCast, JsValue, closure::Closure};

fn failure(msg: &str) -> Error { Error::RustError(msg.into()) }
fn query(sql: &SqlStorage, statement: &str, args: Vec<S>) -> Result<Vec<Value>> {
    sql.exec(statement, args)?.to_array::<Value>()
}
fn crypto(env: &Env, chat: &str, seq: u64, plaintext: Option<&str>,
    encrypted: Option<(&str, &str)>) -> Result<String> {
    let mut key = crate::security::derive_message_key(env, chat, b"api-chat/aes256gcm/v1").map_err(|_| failure("MESSAGE_KEY invalid"))?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| failure("cipher initialization"))?;
    key.fill(0);
    let aad = format!("{chat}:{seq}");
    match plaintext {
        Some(text) => {
            let mut nonce = [0_u8; 12];
            getrandom::fill(&mut nonce).map_err(|_| failure("secure random unavailable"))?;
            let sealed = cipher.encrypt(Nonce::from_slice(&nonce),
                Payload { msg: text.as_bytes(), aad: aad.as_bytes() })
                .map_err(|_| failure("encryption failed"))?;
            Ok(format!("{}.{}", URL_SAFE_NO_PAD.encode(nonce), URL_SAFE_NO_PAD.encode(sealed)))
        }
        None => {
            let (nonce, bytes) = encrypted.ok_or_else(|| failure("ciphertext missing"))?;
            let nonce = URL_SAFE_NO_PAD.decode(nonce).map_err(|_| failure("nonce invalid"))?;
            let bytes = URL_SAFE_NO_PAD.decode(bytes).map_err(|_| failure("ciphertext invalid"))?;
            if nonce.len() != 12 { return Err(failure("nonce invalid")); }
            let opened = cipher.decrypt(Nonce::from_slice(&nonce),
                Payload { msg: &bytes, aad: aad.as_bytes() })
                .map_err(|_| failure("decryption failed"))?;
            String::from_utf8(opened).map_err(|_| failure("invalid utf8"))
        }
    }
}

// SQLite transactionSync must roll back *all* inserts if any step fails.
fn transaction<T: 'static>(storage: &Storage,
    action: impl FnOnce(SqlStorage) -> Result<T> + 'static) -> Result<T> {
    let result = Rc::new(RefCell::new(None));
    let slot = result.clone();
    let sql = storage.sql();
    let callback = Closure::once(move || -> std::result::Result<(), JsValue> {
        let outcome = action(sql);
        let failed = outcome.is_err();
        *slot.borrow_mut() = Some(outcome);
        if failed { Err(JsValue::from_str("transaction rejected")) } else { Ok(()) }
    });
    let raw = storage.as_raw().as_ref();
    let method = worker::js_sys::Reflect::get(raw, &JsValue::from_str("transactionSync"))
        .map_err(Error::from)?.dyn_into::<worker::js_sys::Function>().map_err(Error::from)?;
    let called = method.call1(raw, callback.as_ref());
    let outcome = result.borrow_mut().take().ok_or_else(|| failure("transaction callback not executed"))?;
    if called.is_err() && outcome.is_ok() { return Err(failure("transaction aborted")); }
    outcome
}

fn initialize(storage: &Storage) -> Result<()> {
    let sql = storage.sql();
    for s in [
        "CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT,kind TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS messages(seq INTEGER PRIMARY KEY,chat_id TEXT NOT NULL,sender_id TEXT NOT NULL,client_id TEXT NOT NULL,fingerprint TEXT NOT NULL,nonce TEXT NOT NULL,ciphertext TEXT NOT NULL,created_at INTEGER NOT NULL,UNIQUE(sender_id,client_id))",
        "CREATE TABLE IF NOT EXISTS outbox(seq INTEGER NOT NULL,recipient_id TEXT NOT NULL,PRIMARY KEY(seq,recipient_id))"
    ] { sql.exec(s, None)?; }
    Ok(())
}

#[durable_object]
pub struct ChatRoom {
    state: State,
    env: Env
}

impl ChatRoom {
    fn send(&self, value: &Value) -> Result<Response> {
        let chat = value["chat_id"].as_str().ok_or_else(|| failure("chat missing"))?.to_string();
        let sender = value["sender_id"].as_str().ok_or_else(|| failure("sender missing"))?.to_string();
        let client = value["client_message_id"].as_str().ok_or_else(|| failure("id missing"))?.to_string();
        let text = value["text"].as_str().ok_or_else(|| failure("message missing"))?.to_string();
        let members: Vec<String> = value["recipients"].as_array()
            .ok_or_else(|| failure("recipients missing"))?
            .iter().filter_map(|x| x.as_str().map(String::from)).collect();
        if !crate::security::uuid(&chat) || !crate::security::uuid(&sender) ||
            !crate::security::uuid(&client) || text.is_empty() || text.len() > 16384 ||
            members.is_empty() || members.len() > 100 || !members.contains(&sender) ||
            !members.iter().all(|x| crate::security::uuid(x)) {
            return Response::error("invalid send request", 422);
        }
        let expected = self.env.durable_object("CHAT_ROOM")?.id_from_name(&chat)?;
        if expected.to_string() != self.state.id().to_string() {
            return Response::error("wrong chat target", 403);
        }
        let key = crate::security::derive_message_key(&self.env, &chat, b"api-chat/fingerprint/v1").map_err(|_| failure("key unavailable"))?;
        let mut hmac = <Hmac<Sha256> as Mac>::new_from_slice(&key).map_err(|_| failure("HMAC init"))?;
        hmac.update(b"api-chat/fingerprint/v1");
        hmac.update(chat.as_bytes());
        hmac.update(sender.as_bytes());
        hmac.update(client.as_bytes());
        hmac.update(text.as_bytes());
        let fingerprint = URL_SAFE_NO_PAD.encode(hmac.finalize().into_bytes());
        let env = self.env.clone();
        let cid = chat.clone();
        let result = transaction(&self.state.storage(), move |sql| {
            if let Some(previous) = query(&sql,
                "SELECT seq,fingerprint FROM messages WHERE sender_id=? AND client_id=?",
                vec![sender.clone().into(), client.clone().into()])?.first() {
                if previous["fingerprint"].as_str() != Some(fingerprint.as_str()) {
                    return Err(failure("idempotency conflict"));
                }
                return Ok(json!({"seq":previous["seq"],"duplicate":true}));
            }
            sql.exec("INSERT INTO events(kind) VALUES('message')", None)?;
            let seq = query(&sql, "SELECT last_insert_rowid() AS seq", vec![])?
                .first().and_then(|x| x["seq"].as_u64()).ok_or_else(|| failure("sequence missing"))?;
            let sealed = crypto(&env, &cid, seq, Some(&text), None)?;
            let (nonce, ciphertext) = sealed.split_once('.').ok_or_else(|| failure("encrypted encoding"))?;
            sql.exec("INSERT INTO messages(seq,chat_id,sender_id,client_id,fingerprint,nonce,ciphertext,created_at) VALUES(?,?,?,?,?,?,?,?)",
                vec![(seq as i64).into(),cid.into(),sender.into(),client.into(),
                    fingerprint.into(),nonce.into(),ciphertext.into(),
                    (Date::now().as_millis() as i64).into()])?;
            for recipient in &members {
                sql.exec("INSERT INTO outbox(seq,recipient_id) VALUES(?,?)",
                    vec![(seq as i64).into(), recipient.clone().into()])?;
            }
            Ok(json!({"seq":seq,"duplicate":false}))
        });
        match result {
            Ok(payload) => Response::from_json(&json!({"accepted":true,"chat_id":chat,
                "seq":payload["seq"],"duplicate":payload["duplicate"]})),
            Err(_) => Response::error("write failed or idempotency conflict", 409)
        }
    }

    fn sync(&self, value: &Value) -> Result<Response> {
        let chat = value["chat_id"].as_str().ok_or_else(|| failure("chat missing"))?;
        let expected = self.env.durable_object("CHAT_ROOM")?.id_from_name(chat)?;
        if expected.to_string() != self.state.id().to_string() { return Response::error("wrong chat", 403); }
        let after = value["after"].as_u64().unwrap_or(0);
        let limit = value["limit"].as_u64().unwrap_or(100).clamp(1,200);
        let rows = query(&self.state.storage().sql(),
            "SELECT seq,sender_id,client_id,nonce,ciphertext,created_at FROM messages WHERE seq>? ORDER BY seq LIMIT ?",
            vec![(after as i64).into(), (limit as i64).into()])?;
        let mut messages = Vec::with_capacity(rows.len());
        for row in rows {
            let seq = row["seq"].as_u64().ok_or_else(|| failure("bad sequence"))?;
            let plaintext = crypto(&self.env, chat, seq, None,
                Some((row["nonce"].as_str().ok_or_else(|| failure("bad nonce"))?,
                      row["ciphertext"].as_str().ok_or_else(|| failure("bad ciphertext"))?)))?;
            messages.push(json!({"seq":seq,"sender_id":row["sender_id"],
                "client_message_id":row["client_id"],"text":plaintext,"created_at":row["created_at"]}));
        }
        let next = messages.last().and_then(|x| x["seq"].as_u64()).unwrap_or(after);
        Response::from_json(&json!({"chat_id":chat,"messages":messages,"next":next,
            "has_more":messages.len() == limit as usize}))
    }
}

async fn dispatch(env: &Env, storage: &Storage, chat: &str) -> Result<()> {
    let pending = query(&storage.sql(),
        "SELECT seq,recipient_id FROM outbox ORDER BY seq,recipient_id LIMIT 32", vec![])?;
    for row in pending {
        let Some(seq) = row["seq"].as_u64() else { continue };
        let Some(user) = row["recipient_id"].as_str() else { continue };
        let sent: Result<Response> = async {
            let hub = env.durable_object("USER_HUB")?.id_from_name(user)?.get_stub()?;
            let mut init = RequestInit::new();
            init.with_method(Method::Post);
            init.with_body(Some(json!({"chat_id":chat,"seq":seq,"recipient_id":user}).to_string().into()));
            let headers = Headers::new();
            headers.set("Content-Type", "application/json")?;
            init.with_headers(headers);
            hub.fetch_with_request(Request::new_with_init("https://internal/dispatch", &init)?).await
        }.await;
        if let Ok(response) = sent {
            if response.status_code() == 200 {
                storage.sql().exec("DELETE FROM outbox WHERE seq=? AND recipient_id=?",
                    vec![(seq as i64).into(), user.into()])?;
            }
        }
    }
    if !query(&storage.sql(),"SELECT seq FROM outbox LIMIT 1",vec![])?.is_empty() {
        storage.set_alarm(5000_i64).await?;
    }
    Ok(())
}

impl DurableObject for ChatRoom {
    fn new(state: State, env: Env) -> Self { Self { state, env } }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        initialize(&self.state.storage())?;
        let value: Value = req.json().await?;
        match value["op"].as_str() {
            Some("send") => {
                // Arm recovery first: a crash after SQLite commit cannot strand the outbox.
                if self.state.storage().get_alarm().await?.is_none() {
                    self.state.storage().set_alarm(1000_i64).await?;
                }
                let response = self.send(&value)?;
                if response.status_code() == 200 {
                    self.state.storage().sync().await?;
                }
                Ok(response)
            },
            Some("sync") => self.sync(&value),
            _ => Response::error("unknown operation", 404)
        }
    }

    async fn alarm(&self) -> Result<Response> {
        initialize(&self.state.storage())?;
        // The DO instance id is opaque; derive the chat_id from a queued message.
        let rows = query(&self.state.storage().sql(),
            "SELECT chat_id FROM messages JOIN outbox USING(seq) LIMIT 1", vec![])?;
        if let Some(chat) = rows.first().and_then(|x| x["chat_id"].as_str()) {
            if dispatch(&self.env, &self.state.storage(), chat).await.is_err() {
                self.state.storage().set_alarm(5000_i64).await?;
            }
        }
        Response::empty()
    }
}
