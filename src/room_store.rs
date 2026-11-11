//! Synchronous SQLite operations are enclosed in Cloudflare transactionSync.
//! This prevents an await from interleaving the critical authorization+commit path.
use serde_json::Value;
use std::{cell::RefCell,rc::Rc};
use worker::{Error, Result, SqlStorage, SqlStorageValue as S, Storage};
use worker::wasm_bindgen::{JsCast,JsValue,closure::Closure};

pub fn fail(message:&str)->Error { Error::RustError(message.into()) }
pub fn rows(sql:&SqlStorage,q:&str,args:Vec<S>)->Result<Vec<Value>> {
    sql.exec(q,args)?.to_array::<Value>()
}
pub fn number(sql:&SqlStorage,q:&str,args:Vec<S>)->Result<u64> {
    rows(sql,q,args)?.first().and_then(|r|r["n"].as_u64())
        .ok_or_else(||fail("unexpected SQLite integer"))
}
pub fn member(sql:&SqlStorage,user:&str)->Result<bool> {
    Ok(number(sql,"SELECT COUNT(*) AS n FROM members WHERE user_id=?",
        vec![user.into()])?>0)
}
pub fn initialized(sql:&SqlStorage)->Result<bool> {
    Ok(number(sql,"SELECT COUNT(*) AS n FROM room",vec![])?>0)
}

pub fn initialize(storage:&Storage)->Result<()> {
    let sql=storage.sql();
    for statement in [
        "CREATE TABLE IF NOT EXISTS room(chat_id TEXT PRIMARY KEY NOT NULL,epoch INTEGER NOT NULL,rotation_target INTEGER,prune_before_ms INTEGER)",
        "CREATE TABLE IF NOT EXISTS members(user_id TEXT PRIMARY KEY NOT NULL,joined_seq INTEGER NOT NULL DEFAULT 0)",
        "CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY AUTOINCREMENT,kind TEXT NOT NULL)",
        "CREATE TABLE IF NOT EXISTS messages(seq INTEGER PRIMARY KEY NOT NULL,sender_id TEXT NOT NULL,client_id TEXT NOT NULL,fingerprint TEXT NOT NULL,key_version INTEGER NOT NULL,nonce TEXT NOT NULL,ciphertext TEXT NOT NULL,created_ms INTEGER NOT NULL,UNIQUE(sender_id,client_id))",
        "CREATE TABLE IF NOT EXISTS receipts(user_id TEXT PRIMARY KEY NOT NULL,delivered_seq INTEGER NOT NULL DEFAULT 0,read_seq INTEGER NOT NULL DEFAULT 0)",
        "CREATE TABLE IF NOT EXISTS receipt_events(seq INTEGER PRIMARY KEY NOT NULL,user_id TEXT NOT NULL,delivered_seq INTEGER NOT NULL,read_seq INTEGER NOT NULL)",
        "CREATE TABLE IF NOT EXISTS outbox(seq INTEGER NOT NULL,recipient_id TEXT NOT NULL,kind TEXT NOT NULL,attempts INTEGER NOT NULL DEFAULT 0,next_due_ms INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(seq,recipient_id,kind))",
        "CREATE INDEX IF NOT EXISTS outbox_due ON outbox(next_due_ms,seq)",
        "CREATE TABLE IF NOT EXISTS member_index_outbox(user_id TEXT PRIMARY KEY NOT NULL,revision INTEGER NOT NULL,active INTEGER NOT NULL)"
    ] {sql.exec(statement,None)?;}
    Ok(())
}

pub fn event(sql:&SqlStorage,kind:&str)->Result<u64> {
    sql.exec("INSERT INTO events(kind) VALUES(?)",vec![kind.into()])?;
    number(sql,"SELECT last_insert_rowid() AS n",vec![])
}

pub fn enqueue(sql:&SqlStorage,seq:u64,kind:&str,recipient:&str)->Result<()> {
    sql.exec("INSERT OR IGNORE INTO outbox(seq,recipient_id,kind) VALUES(?,?,?)",
        vec![(seq as i64).into(),recipient.into(),kind.into()])?;
    Ok(())
}

pub fn enqueue_members(sql:&SqlStorage,seq:u64,kind:&str)->Result<()> {
    let list=rows(sql,"SELECT user_id FROM members",vec![])?;
    for row in list {
        if let Some(user)=row["user_id"].as_str() {enqueue(sql,seq,kind,user)?;}
    }
    Ok(())
}

pub fn transaction<T:'static>(storage:&Storage,op:impl FnOnce(SqlStorage)->Result<T>+'static)->Result<T> {
    let output=Rc::new(RefCell::new(None));
    let slot=output.clone();
    let sql=storage.sql();
    let callback=Closure::once(move||->std::result::Result<(),JsValue>{
        let result=op(sql);
        let failed=result.is_err();
        *slot.borrow_mut()=Some(result);
        if failed {Err(JsValue::from_str("SQLite transaction rolled back"))} else {Ok(())}
    });
    let raw=storage.as_raw().as_ref();
    let method=worker::js_sys::Reflect::get(raw,&JsValue::from_str("transactionSync"))
        .map_err(Error::from)?.dyn_into::<worker::js_sys::Function>().map_err(Error::from)?;
    let called=method.call1(raw,callback.as_ref());
    let outcome=output.borrow_mut().take().ok_or_else(||fail("transaction callback missing"))?;
    if let Err(err)=called && outcome.is_ok(){return Err(Error::from(err));}
    outcome
}
