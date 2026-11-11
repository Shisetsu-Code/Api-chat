//! Typed wrappers for D1. SQL parameters are always bound, never interpolated.
use serde_json::Value;
use worker::{D1Database, D1PreparedStatement, Env, Result, wasm_bindgen::JsValue};

pub struct Db(pub D1Database);
impl Db {
    pub fn from_env(env: &Env) -> Result<Self> {
        Ok(Self(env.d1("DB")?))
    }

    pub fn bind(&self, sql: &str, values: &[Value]) -> Result<D1PreparedStatement> {
        let args: Vec<JsValue> = values
            .iter()
            .map(|v| match v {
                Value::Null => JsValue::NULL,
                Value::Bool(b) => JsValue::from_f64(if *b { 1.0 } else { 0.0 }),
                Value::Number(n) => JsValue::from_f64(n.as_f64().unwrap_or_default()),
                Value::String(s) => JsValue::from_str(s),
                _ => JsValue::from_str(&v.to_string()),
            })
            .collect();
        self.0.prepare(sql).bind(&args)
    }

    pub async fn first(&self, sql: &str, values: &[Value]) -> Result<Option<Value>> {
        self.bind(sql, values)?.first(None).await
    }
    pub async fn all(&self, sql: &str, values: &[Value]) -> Result<Vec<Value>> {
        self.bind(sql, values)?.all().await?.results()
    }
    pub async fn execute(&self, sql: &str, values: &[Value]) -> Result<()> {
        self.bind(sql, values)?.run().await.map(|_| ())
    }
}
