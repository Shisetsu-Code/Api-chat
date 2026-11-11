//! Authoritative conversation actor.
//! Messages, receipts, membership and pending fanout live in one SQLite database.
//! Mutations are synchronous SQL transactions; outbox delivery is at-least-once.
use crate::{db::Db, room_crypto as crypto, room_store as store, security};
use serde_json::{Value, json};
use std::{cell::Cell, rc::Rc};
use worker::*;

fn data_error(code: &str, status: u16) -> Result<Response> {
    Ok(Response::from_json(&json!({"error":code}))?.with_status(status))
}
fn response(v: &Value, status: u16) -> Result<Response> {
    let mut result = Response::from_json(v)?.with_status(status);
    result.headers_mut().set("Cache-Control", "no-store")?;
    Ok(result)
}
fn chat_of(v: &Value) -> Option<&str> {
    v["chat_id"].as_str().filter(|s| security::uuid(s))
}
fn message_user(v: &Value) -> Option<&str> {
    v["user_id"].as_str().filter(|s| security::uuid(s))
}
fn target_id(v: &Value) -> Option<&str> {
    v["member_id"].as_str().filter(|s| security::uuid(s))
}
fn safe_seq(v: &Value) -> Option<u64> {
    v.as_u64().filter(|x| *x <= 9_007_199_254_740_991)
}
#[durable_object]
pub struct ChatRoom {
    state: State,
    env: Env,
    fast_dispatching: Rc<Cell<bool>>,
}
impl ChatRoom {
    fn valid_target(&self, chat: &str) -> Result<bool> {
        Ok(self.state.id().to_string()
            == self
                .env
                .durable_object("CHAT_ROOM")?
                .id_from_name(chat)?
                .to_string())
    }
    fn current_room(&self, chat: &str) -> Result<bool> {
        Ok(store::rows(
            &self.state.storage().sql(),
            "SELECT chat_id FROM room WHERE chat_id=?",
            vec![chat.into()],
        )?
        .len()
            == 1)
    }
    async fn rearm_if_needed(&self) -> Result<()> {
        // This is deliberately *before* the write: the wakeup is persisted before
        // the transaction can create durable outbox rows.
        if self.state.storage().get_alarm().await?.is_none() {
            self.state.storage().set_alarm(30_000_i64).await?;
        }
        Ok(())
    }
    async fn wake_soon(&self) -> Result<()> {
        // Cloudflare only stores one alarm. Replacing it after every send can
        // indefinitely postpone delivery during sustained message traffic.
        let due = (worker::Date::now().as_millis() as i64).saturating_add(1_000);
        let storage = self.state.storage();
        if storage
            .get_alarm()
            .await?
            .is_none_or(|existing| existing > due)
        {
            storage.set_alarm(1_000_i64).await?;
        }
        Ok(())
    }

    fn start_fast_dispatch(&self, chat: &str) {
        if self.fast_dispatching.replace(true) {
            return;
        }
        let flag = self.fast_dispatching.clone();
        let env = self.env.clone();
        let storage = self.state.storage();
        let chat = chat.to_owned();
        // This future runs after the commit and does NOT delay ACCEPTED ACK.
        // The durable outbox + pre-commit alarm remain the recovery authority.
        self.state.wait_until(async move {
            // Move any slower fallback alarm forward without postponing an
            // existing earlier alarm. The fast path is not the sole retry.
            let due = (worker::Date::now().as_millis() as i64).saturating_add(1_000);
            match storage.get_alarm().await {
                Ok(current) if current.is_none_or(|previous| previous > due) => {
                    if let Err(error) = storage.set_alarm(1_000_i64).await {
                        console_error!("fast-dispatch fallback alarm failed: {:?}", error);
                    }
                }
                Err(error) => console_error!("fast-dispatch alarm lookup failed: {:?}", error),
                _ => {}
            }
            // Bounded draining prevents a hot room from accumulating a
            // separate unbounded delivery future for every message.
            for _ in 0..16 {
                match ChatRoom::deliver_batch(&env, &storage, &chat).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) => {
                        console_error!(
                            "fast-dispatch failed, durable outbox retained: {:?}",
                            error
                        );
                        break;
                    }
                }
            }
            if let Err(error) = storage.sync().await {
                console_error!("fast-dispatch storage flush failed: {:?}", error);
            }
            flag.set(false);
        });
    }
    async fn create(&self, v: &Value) -> Result<Response> {
        let Some(chat) = chat_of(v) else {
            return data_error("invalid_chat_id", 422);
        };
        let Some(members) = v["members"].as_array() else {
            return data_error("members_required", 422);
        };
        let ids: Vec<String> = members
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
        if !(2..=100).contains(&ids.len())
            || unique.len() != ids.len()
            || !ids.iter().all(|s| security::uuid(s))
        {
            return data_error("invalid_members", 422);
        }
        let initial_version =
            security::version(&self.env, "ACTIVE_MESSAGE_KEY_VERSION").map_err(Error::RustError)?;
        if security::derive_message_key(&self.env, chat, initial_version).is_err() {
            return data_error("encryption_key_unavailable", 503);
        }
        self.rearm_if_needed().await?;
        let chat_owned = chat.to_string();
        let value = store::transaction(&self.state.storage(), move |sql| {
            if store::initialized(&sql)? {
                let present =
                    store::rows(&sql, "SELECT user_id FROM members ORDER BY user_id", vec![])?;
                let current: std::collections::BTreeSet<String> = present
                    .into_iter()
                    .filter_map(|r| r["user_id"].as_str().map(str::to_string))
                    .collect();
                let requested: std::collections::BTreeSet<String> = ids.iter().cloned().collect();
                return Ok(json!({"created":false,"same_members":current==requested}));
            }
            sql.exec(
                "INSERT INTO room(chat_id,epoch,active_key_version) VALUES(?,1,?)",
                vec![chat_owned.into(), (initial_version as i64).into()],
            )?;
            let event = store::event(&sql, "membership")?;
            for user in &ids {
                sql.exec(
                    "INSERT INTO members(user_id,joined_seq) VALUES(?,0)",
                    vec![user.clone().into()],
                )?;
                sql.exec(
                    "INSERT INTO member_index_outbox(user_id,revision,active) VALUES(?,?,1)",
                    vec![user.clone().into(), (event as i64).into()],
                )?;
                store::enqueue(&sql, event, "membership", user)?;
            }
            Ok(json!({"created":true,"revision":event,"same_members":true}))
        })?;
        if value["same_members"] != true {
            return data_error("chat_already_exists", 409);
        }
        self.state.storage().sync().await?;
        self.wake_soon().await?;
        response(
            &json!({"chat_id":chat,"revision":value["revision"],
            "created":value["created"],"authority":"durable-object"}),
            201,
        )
    }
    async fn mutate_member(&self, v: &Value, add: bool) -> Result<Response> {
        let (Some(chat), Some(target)) = (chat_of(v), target_id(v)) else {
            return data_error("invalid_member", 422);
        };
        if !self.current_room(chat)? {
            return data_error("chat_not_found", 404);
        }
        self.rearm_if_needed().await?;
        let id = target.to_string();
        let action = store::transaction(&self.state.storage(), move |sql| {
            let already = store::member(&sql, &id)?;
            if already == add {
                return Ok(json!({"changed":false,"added":add}));
            }
            if add && store::number(&sql, "SELECT COUNT(*) AS n FROM members", vec![])? >= 100 {
                return Ok(json!({"error":"chat_full"}));
            }
            let revision = store::event(&sql, "membership")?;
            if add {
                sql.exec(
                    "INSERT INTO members(user_id,joined_seq) VALUES(?,?)",
                    vec![id.clone().into(), (revision as i64).into()],
                )?;
            } else {
                sql.exec(
                    "DELETE FROM members WHERE user_id=?",
                    vec![id.clone().into()],
                )?;
                sql.exec(
                    "DELETE FROM receipts WHERE user_id=?",
                    vec![id.clone().into()],
                )?;
                sql.exec(
                    "DELETE FROM outbox WHERE recipient_id=? AND kind IN ('message','receipt')",
                    vec![id.clone().into()],
                )?;
            }
            sql.exec("INSERT INTO member_index_outbox(user_id,revision,active) VALUES(?,?,?) ON CONFLICT(user_id) DO UPDATE SET revision=excluded.revision,active=excluded.active",
                vec![id.clone().into(),(revision as i64).into(),(if add{1_i64}else{0_i64}).into()])?;
            store::enqueue_members(&sql, revision, "membership")?;
            // Even a removed member receives a revocation notification (no plaintext).
            if !add {
                store::enqueue(&sql, revision, "membership", &id)?;
            }
            Ok(json!({"changed":true,"added":add,"revision":revision}))
        })?;
        if let Some(code) = action["error"].as_str() {
            return data_error(code, 409);
        }
        self.state.storage().sync().await?;
        self.wake_soon().await?;
        response(
            &json!({"chat_id":chat,"member_id":target,"result":action}),
            200,
        )
    }
    async fn send(&self, v: &Value) -> Result<Response> {
        let (Some(chat), Some(user), Some(client), Some(text)) = (
            chat_of(v),
            message_user(v),
            v["client_id"].as_str().filter(|s| security::uuid(s)),
            v["text"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 16_384),
        ) else {
            return data_error("invalid_message", 422);
        };
        if !self.current_room(chat)? {
            return data_error("chat_not_found", 404);
        }
        let fingerprint =
            security::fingerprint(&self.env, chat, user, client, text).map_err(Error::RustError)?;
        self.rearm_if_needed().await?;
        let (env, chat_id, sender, client_id, plain) = (
            self.env.clone(),
            chat.to_string(),
            user.to_string(),
            client.to_string(),
            text.to_string(),
        );
        let outcome = store::transaction(&self.state.storage(), move |sql| {
            if !store::member(&sql, &sender)? {
                return Ok(json!({"error":"not_a_member"}));
            }
            if let Some(previous) = store::rows(
                &sql,
                "SELECT seq,fingerprint FROM messages WHERE sender_id=? AND client_id=?",
                vec![sender.clone().into(), client_id.clone().into()],
            )?
            .first()
            {
                if previous["fingerprint"].as_str() != Some(fingerprint.as_str()) {
                    return Ok(json!({"error":"idempotency_conflict"}));
                }
                return Ok(json!({"accepted":true,"seq":previous["seq"],"duplicate":true}));
            }
            let key_info =
                store::rows(&sql, "SELECT active_key_version FROM room LIMIT 1", vec![])?;
            let key_version = key_info
                .first()
                .and_then(|r| r["active_key_version"].as_u64())
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(|| store::fail("missing key version"))?;
            let seq = store::event(&sql, "message")?;
            let (nonce, ciphertext) = crypto::seal(&env, &chat_id, seq, key_version, &plain)?;
            sql.exec("INSERT INTO messages(seq,sender_id,client_id,fingerprint,key_version,nonce,ciphertext,created_ms) VALUES(?,?,?,?,?,?,?,?)",
                vec![(seq as i64).into(),sender.into(),client_id.into(),fingerprint.into(),
                     (key_version as i64).into(),nonce.into(),ciphertext.into(),security::now_ms().into()])?;
            store::enqueue_members(&sql, seq, "message")?;
            Ok(json!({"accepted":true,"seq":seq,"duplicate":false}))
        })?;
        match outcome["error"].as_str() {
            Some("not_a_member") => return data_error("not_a_member", 403),
            Some("idempotency_conflict") => return data_error("idempotency_conflict", 409),
            _ => {}
        }
        self.state.storage().sync().await?;
        if outcome["duplicate"] != true {
            self.start_fast_dispatch(chat);
        }
        response(
            &json!({"chat_id":chat,"accepted":true,"seq":outcome["seq"],
            "duplicate":outcome["duplicate"]}),
            200,
        )
    }
    fn sync(&self, v: &Value) -> Result<Response> {
        let (Some(chat), Some(user)) = (chat_of(v), message_user(v)) else {
            return data_error("invalid_request", 422);
        };
        let sql = self.state.storage().sql();
        let membership = store::rows(
            &sql,
            "SELECT joined_seq FROM members WHERE user_id=?",
            vec![user.into()],
        )?;
        let Some(member) = membership.first() else {
            return data_error("not_a_member", 403);
        };
        let joined = member["joined_seq"].as_u64().unwrap_or(0);
        let Some(after) = safe_seq(&v["after"]) else {
            return data_error("invalid_cursor", 422);
        };
        let limit = v["limit"].as_u64().unwrap_or(100);
        if !(1..=200).contains(&limit) {
            return data_error("invalid_limit", 422);
        }
        let start = after.max(joined);
        let events = store::rows(
            &sql,
            "SELECT seq,kind FROM events WHERE seq>? ORDER BY seq LIMIT ?",
            vec![(start as i64).into(), ((limit + 1) as i64).into()],
        )?;
        let has_more = events.len() > limit as usize;
        let mut changes = Vec::new();
        let mut next = start;
        for event in events.into_iter().take(limit as usize) {
            let seq = event["seq"]
                .as_u64()
                .ok_or_else(|| store::fail("event sequence invalid"))?;
            next = seq;
            match event["kind"].as_str().unwrap_or_default() {
                "message" => {
                    let rows = store::rows(
                        &sql,
                        "SELECT sender_id,client_id,key_version,nonce,ciphertext,created_ms FROM messages WHERE seq=?",
                        vec![(seq as i64).into()],
                    )?;
                    if let Some(row) = rows.first() {
                        let version = row["key_version"]
                            .as_u64()
                            .and_then(|n| u8::try_from(n).ok())
                            .ok_or_else(|| store::fail("key version invalid"))?;
                        let plain = crypto::open(
                            &self.env,
                            chat,
                            seq,
                            version,
                            row["nonce"]
                                .as_str()
                                .ok_or_else(|| store::fail("nonce missing"))?,
                            row["ciphertext"]
                                .as_str()
                                .ok_or_else(|| store::fail("ciphertext missing"))?,
                        )?;
                        changes.push(json!({"type":"message","seq":seq,
                            "sender_id":row["sender_id"],"client_message_id":row["client_id"],
                            "text":plain,"created_ms":row["created_ms"]}));
                    } else {
                        changes.push(json!({"type":"message.pruned","seq":seq}));
                    }
                }
                "receipt" => {
                    let entries = store::rows(
                        &sql,
                        "SELECT user_id,delivered_seq,read_seq FROM receipt_events WHERE seq=?",
                        vec![(seq as i64).into()],
                    )?;
                    if let Some(row) = entries.first() {
                        changes.push(json!({"type":"receipt","seq":seq,"user_id":row["user_id"],
                            "delivered_seq":row["delivered_seq"],"read_seq":row["read_seq"]}));
                    }
                }
                "membership" => {
                    changes.push(json!({"type":"membership.changed","seq":seq}));
                }
                "pruned" => {
                    changes.push(json!({"type":"message.pruned","seq":seq}));
                }
                _ => {}
            }
        }
        response(
            &json!({"chat_id":chat,"changes":changes,"next":next,
            "has_more":has_more,"membership_revision":joined}),
            200,
        )
    }
    async fn receipt(&self, v: &Value) -> Result<Response> {
        let (Some(_chat), Some(user)) = (chat_of(v), message_user(v)) else {
            return data_error("invalid_request", 422);
        };
        let delivered = v["delivered_seq"].as_u64();
        let read = v["read_seq"].as_u64();
        if delivered.is_none() && read.is_none() {
            return data_error("receipt_cursor_required", 422);
        }
        if [delivered, read]
            .into_iter()
            .flatten()
            .any(|x| x > 9_007_199_254_740_991)
        {
            return data_error("invalid_cursor", 422);
        }
        self.rearm_if_needed().await?;
        let who = user.to_string();
        let outcome = store::transaction(&self.state.storage(), move |sql| {
            if !store::member(&sql, &who)? {
                return Ok(json!({"error":"not_a_member"}));
            }
            let existing = store::rows(
                &sql,
                "SELECT delivered_seq,read_seq FROM receipts WHERE user_id=?",
                vec![who.clone().into()],
            )?;
            let previous_delivered = existing
                .first()
                .and_then(|r| r["delivered_seq"].as_u64())
                .unwrap_or(0);
            let previous_read = existing
                .first()
                .and_then(|r| r["read_seq"].as_u64())
                .unwrap_or(0);
            let d = delivered
                .unwrap_or(previous_delivered)
                .max(previous_delivered);
            let r = read.unwrap_or(previous_read).max(previous_read);
            if r > d {
                return Ok(json!({"error":"read_exceeds_delivered"}));
            }
            for claimed in [d, r].into_iter().filter(|x| *x > 0) {
                if store::number(
                    &sql,
                    "SELECT COUNT(*) AS n FROM messages WHERE seq=?",
                    vec![(claimed as i64).into()],
                )? == 0
                {
                    return Ok(json!({"error":"unknown_message_cursor"}));
                }
            }
            if d == previous_delivered && r == previous_read {
                return Ok(json!({"changed":false,"delivered_seq":d,"read_seq":r}));
            }
            sql.exec("INSERT INTO receipts(user_id,delivered_seq,read_seq) VALUES(?,?,?) ON CONFLICT(user_id) DO UPDATE SET delivered_seq=excluded.delivered_seq,read_seq=excluded.read_seq",
                vec![who.clone().into(),(d as i64).into(),(r as i64).into()])?;
            let seq = store::event(&sql, "receipt")?;
            sql.exec(
                "INSERT INTO receipt_events(seq,user_id,delivered_seq,read_seq) VALUES(?,?,?,?)",
                vec![
                    (seq as i64).into(),
                    who.into(),
                    (d as i64).into(),
                    (r as i64).into(),
                ],
            )?;
            store::enqueue_members(&sql, seq, "receipt")?;
            Ok(json!({"changed":true,"seq":seq,"delivered_seq":d,"read_seq":r}))
        })?;
        if let Some(code) = outcome["error"].as_str() {
            return data_error(code, if code == "not_a_member" { 403 } else { 422 });
        }
        self.state.storage().sync().await?;
        self.wake_soon().await?;
        response(&outcome, 200)
    }
    fn receipts_get(&self, v: &Value) -> Result<Response> {
        let (Some(_chat), Some(user)) = (chat_of(v), message_user(v)) else {
            return data_error("invalid_request", 422);
        };
        let sql = self.state.storage().sql();
        if !store::member(&sql, user)? {
            return data_error("not_a_member", 403);
        }
        let rows = store::rows(
            &sql,
            "SELECT user_id,delivered_seq,read_seq FROM receipts ORDER BY user_id",
            vec![],
        )?;
        response(&json!({"receipts":rows}), 200)
    }
    async fn rotation_start(&self, v: &Value) -> Result<Response> {
        let Some(target) = v["target_key_version"]
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .filter(|v| (1..=2).contains(v))
        else {
            return data_error("invalid_target_version", 422);
        };
        let Some(chat) = chat_of(v) else {
            return data_error("invalid_chat_id", 422);
        };
        if !self.current_room(chat)? {
            return data_error("chat_not_found", 404);
        }
        if security::derive_message_key(&self.env, chat, target).is_err() {
            return data_error("encryption_key_unavailable", 503);
        }
        self.rearm_if_needed().await?;
        self.state.storage().sql().exec(
            "UPDATE room SET rotation_target=?,active_key_version=?",
            vec![(target as i64).into(), (target as i64).into()],
        )?;
        self.state.storage().sync().await?;
        self.wake_soon().await?;
        response(
            &json!({"rotation_requested":true,"target_key_version":target}),
            202,
        )
    }
    async fn prune_start(&self, v: &Value) -> Result<Response> {
        let Some(before) = v["before_ms"]
            .as_i64()
            .filter(|n| *n > 0 && *n < security::now_ms())
        else {
            return data_error("invalid_before_ms", 422);
        };
        let Some(chat) = chat_of(v) else {
            return data_error("invalid_chat_id", 422);
        };
        if !self.current_room(chat)? {
            return data_error("chat_not_found", 404);
        }
        self.rearm_if_needed().await?;
        self.state
            .storage()
            .sql()
            .exec("UPDATE room SET prune_before_ms=?", vec![before.into()])?;
        self.state.storage().sync().await?;
        self.wake_soon().await?;
        response(&json!({"pruning_requested":true,"before_ms":before}), 202)
    }
    fn work_exists(storage: &Storage) -> Result<bool> {
        let sql = storage.sql();
        for query in [
            "SELECT COUNT(*) AS n FROM outbox",
            "SELECT COUNT(*) AS n FROM member_index_outbox",
            "SELECT COUNT(*) AS n FROM room WHERE rotation_target IS NOT NULL",
            "SELECT COUNT(*) AS n FROM room WHERE prune_before_ms IS NOT NULL",
        ] {
            if store::number(&sql, query, vec![])? > 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }
    async fn repair_projection(&self, chat: &str) -> Result<()> {
        let sql = self.state.storage().sql();
        let jobs = store::rows(
            &sql,
            "SELECT user_id,revision,active FROM member_index_outbox LIMIT 16",
            vec![],
        )?;
        let db = Db::from_env(&self.env)?;
        for item in jobs {
            let Some(user) = item["user_id"].as_str() else {
                continue;
            };
            let rev = item["revision"].as_u64().unwrap_or(0);
            let active = item["active"].as_u64().unwrap_or(0);
            db.execute("INSERT INTO chat_member_index(chat_id,user_id,revision,active) VALUES(?1,?2,?3,?4) ON CONFLICT(chat_id,user_id) DO UPDATE SET revision=excluded.revision,active=excluded.active WHERE excluded.revision>=chat_member_index.revision",
                &[json!(chat),json!(user),json!(rev),json!(active)]).await?;
            sql.exec(
                "DELETE FROM member_index_outbox WHERE user_id=? AND revision=?",
                vec![user.into(), (rev as i64).into()],
            )?;
        }
        Ok(())
    }
    async fn rotate_batch(&self, chat: &str) -> Result<()> {
        let sql = self.state.storage().sql();
        let room = store::rows(&sql, "SELECT rotation_target FROM room LIMIT 1", vec![])?;
        let Some(target) = room
            .first()
            .and_then(|r| r["rotation_target"].as_u64())
            .and_then(|v| u8::try_from(v).ok())
        else {
            return Ok(());
        };
        let pending = store::rows(
            &sql,
            "SELECT seq,key_version,nonce,ciphertext FROM messages WHERE key_version<>? ORDER BY seq LIMIT 16",
            vec![(target as i64).into()],
        )?;
        if pending.is_empty() {
            sql.exec("UPDATE room SET rotation_target=NULL", None)?;
            return Ok(());
        }
        for item in pending {
            let seq = item["seq"]
                .as_u64()
                .ok_or_else(|| store::fail("bad sequence"))?;
            let old = item["key_version"]
                .as_u64()
                .and_then(|v| u8::try_from(v).ok())
                .ok_or_else(|| store::fail("bad key version"))?;
            let plain = crypto::open(
                &self.env,
                chat,
                seq,
                old,
                item["nonce"]
                    .as_str()
                    .ok_or_else(|| store::fail("bad nonce"))?,
                item["ciphertext"]
                    .as_str()
                    .ok_or_else(|| store::fail("bad ciphertext"))?,
            )?;
            let (nonce, ct) = crypto::seal(&self.env, chat, seq, target, &plain)?;
            sql.exec("UPDATE messages SET key_version=?,nonce=?,ciphertext=? WHERE seq=? AND key_version=?",
                vec![(target as i64).into(),nonce.into(),ct.into(),
                     (seq as i64).into(),(old as i64).into()])?;
        }
        Ok(())
    }
    fn prune_batch(&self) -> Result<()> {
        let sql = self.state.storage().sql();
        let pending = store::rows(
            &sql,
            "SELECT prune_before_ms FROM room WHERE prune_before_ms IS NOT NULL",
            vec![],
        )?;
        let Some(before) = pending.first().and_then(|r| r["prune_before_ms"].as_i64()) else {
            return Ok(());
        };
        let items = store::rows(
            &sql,
            "SELECT seq FROM messages WHERE created_ms<? ORDER BY seq LIMIT 100",
            vec![before.into()],
        )?;
        if items.is_empty() {
            sql.exec("UPDATE room SET prune_before_ms=NULL", None)?;
            return Ok(());
        }
        for item in items {
            let seq = item["seq"]
                .as_u64()
                .ok_or_else(|| store::fail("bad prune sequence"))?;
            sql.exec(
                "DELETE FROM messages WHERE seq=?",
                vec![(seq as i64).into()],
            )?;
            sql.exec(
                "DELETE FROM outbox WHERE seq=? AND kind='message'",
                vec![(seq as i64).into()],
            )?;
            sql.exec(
                "UPDATE events SET kind='pruned' WHERE seq=?",
                vec![(seq as i64).into()],
            )?;
        }
        Ok(())
    }
    async fn deliver_batch(env: &Env, storage: &Storage, chat: &str) -> Result<usize> {
        let sql = storage.sql();
        let jobs = store::rows(
            &sql,
            "SELECT seq,recipient_id,kind,attempts FROM outbox WHERE next_due_ms<=? ORDER BY seq LIMIT 32",
            vec![security::now_ms().into()],
        )?;
        let processed = jobs.len();
        for job in jobs {
            let seq = job["seq"].as_u64().unwrap_or(0);
            let Some(recipient) = job["recipient_id"].as_str() else {
                continue;
            };
            let Some(kind) = job["kind"].as_str() else {
                continue;
            };
            // Remove stale notifications to former members except membership revocation.
            if kind != "membership" && !store::member(&sql, recipient)? {
                sql.exec(
                    "DELETE FROM outbox WHERE seq=? AND recipient_id=? AND kind=?",
                    vec![(seq as i64).into(), recipient.into(), kind.into()],
                )?;
                continue;
            }
            let result: Result<Response> = async {
                let stub = env
                    .durable_object("USER_HUB")?
                    .id_from_name(recipient)?
                    .get_stub()?;
                let mut init = RequestInit::new();
                init.with_method(Method::Post);
                let payload = json!({"op":"notify","chat_id":chat,"seq":seq,
                    "recipient_id":recipient,"kind":kind});
                init.with_body(Some(
                    serde_json::to_string(&payload)
                        .map_err(|_| store::fail("relay serialization failed"))?
                        .into(),
                ));
                let headers = Headers::new();
                headers.set("Content-Type", "application/json")?;
                init.with_headers(headers);
                stub.fetch_with_request(Request::new_with_init("https://internal/dispatch", &init)?)
                    .await
            }
            .await;
            if result.is_ok_and(|resp| resp.status_code() == 200) {
                sql.exec(
                    "DELETE FROM outbox WHERE seq=? AND recipient_id=? AND kind=?",
                    vec![(seq as i64).into(), recipient.into(), kind.into()],
                )?;
            } else {
                let n = job["attempts"].as_u64().unwrap_or(0).min(20);
                let backoff = (1_i64 << n.min(6)) * 1000;
                sql.exec("UPDATE outbox SET attempts=attempts+1,next_due_ms=? WHERE seq=? AND recipient_id=? AND kind=?",
                    vec![(security::now_ms()+backoff.min(60_000)).into(),
                        (seq as i64).into(),recipient.into(),kind.into()])?;
            }
        }
        Ok(processed)
    }
}
impl DurableObject for ChatRoom {
    fn new(state: State, env: Env) -> Self {
        Self {
            state,
            env,
            fast_dispatching: Rc::new(Cell::new(false)),
        }
    }
    async fn fetch(&self, mut req: Request) -> Result<Response> {
        let result: Result<Response> = async {
            if req.method() != Method::Post {
                return data_error("method_not_allowed", 405);
            }
            let v: Value = req.json().await?;
            store::initialize(&self.state.storage())?;
            let Some(chat) = chat_of(&v) else {
                return data_error("invalid_chat_id", 422);
            };
            if !self.valid_target(chat)? {
                return data_error("wrong_chat_target", 403);
            }
            match v["op"].as_str() {
                Some("create") => self.create(&v).await,
                Some("member.add") => self.mutate_member(&v, true).await,
                Some("member.remove") => self.mutate_member(&v, false).await,
                Some("send") => self.send(&v).await,
                Some("sync") => self.sync(&v),
                Some("receipt") => self.receipt(&v).await,
                Some("receipts.get") => self.receipts_get(&v),
                Some("rotate") => self.rotation_start(&v).await,
                Some("prune") => self.prune_start(&v).await,
                _ => data_error("unknown_operation", 404),
            }
        }
        .await;
        match result {
            Ok(reply) => Ok(reply),
            Err(e) => {
                console_error!("ChatRoom failed: {:?}", e);
                data_error("room_internal_failure", 503)
            }
        }
    }
    async fn alarm(&self) -> Result<Response> {
        store::initialize(&self.state.storage())?;
        let sql = self.state.storage().sql();
        let current = store::rows(&sql, "SELECT chat_id FROM room LIMIT 1", vec![])?;
        let Some(chat) = current.first().and_then(|v| v["chat_id"].as_str()) else {
            return Response::empty();
        };
        // Persist a recovery wakeup before accessing other services.
        self.state.storage().set_alarm(30_000_i64).await?;
        let mut error = false;
        if self.repair_projection(chat).await.is_err() {
            error = true;
        }
        if self.rotate_batch(chat).await.is_err() {
            error = true;
        }
        if self.prune_batch().is_err() {
            error = true;
        }
        if ChatRoom::deliver_batch(&self.env, &self.state.storage(), chat)
            .await
            .is_err()
        {
            error = true;
        }
        self.state.storage().sync().await?;
        // Atomically decide the next alarm against any concurrent incoming send.
        // The short gate covers local storage operations only (never D1/fetch).
        let storage = self.state.storage();
        self.state
            .block_concurrency_while(async move {
                let busy = error || ChatRoom::work_exists(&storage)?;
                if busy {
                    storage.set_alarm(5_000_i64).await?;
                } else {
                    storage.delete_alarm().await?;
                }
                Ok(())
            })
            .await?;
        Response::empty()
    }
}
