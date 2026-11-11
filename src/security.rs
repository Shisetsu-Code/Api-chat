use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use worker::{Date, Env};

type HmacSha256 = Hmac<Sha256>;

#[derive(Serialize, Deserialize)]
struct Claims {
    sub: String,
    exp: u64,
}

pub fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok()
}

pub fn admin(env: &Env, provided: Option<String>) -> bool {
    let Ok(secret) = env.secret("ADMIN_KEY") else { return false };
    let Some(provided) = provided else { return false };
    let key = secret.to_string();
    !key.is_empty() && key.as_bytes().ct_eq(provided.as_bytes()).into()
}

pub fn issue(env: &Env, user: &str, ttl: u64) -> Result<String, String> {
    if !uuid(user) || !(1..=86400).contains(&ttl) {
        return Err("invalid token request".into());
    }
    let payload = serde_json::to_vec(&Claims {
        sub: user.into(),
        exp: Date::now().as_millis() as u64 / 1000 + ttl,
    }).map_err(|_| "serialization")?;
    let encoded = URL_SAFE_NO_PAD.encode(payload);
    let signature = mac(env, encoded.as_bytes())?;
    Ok(format!("{encoded}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

fn mac(env: &Env, data: &[u8]) -> Result<Vec<u8>, String> {
    let secret = env.secret("AUTH_KEY").map_err(|_| "missing auth key")?.to_string();
    let key = URL_SAFE_NO_PAD.decode(secret).map_err(|_| "invalid auth key")?;
    if key.len() != 32 { return Err("invalid auth key length".into()); }
    let mut h = <HmacSha256 as Mac>::new_from_slice(&key).map_err(|_| "hmac init")?;
    h.update(data);
    Ok(h.finalize().into_bytes().to_vec())
}

pub fn verify(env: &Env, header: Option<String>) -> Option<String> {
    let header = header?.strip_prefix("Bearer ")?.to_string();
    if header.len() > 2048 { return None; }
    let (payload, signature) = header.split_once('.')?;
    let actual = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let expected = mac(env, payload.as_bytes()).ok()?;
    if actual.len() != expected.len() || !bool::from(actual.ct_eq(&expected)) { return None; }
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Claims = serde_json::from_slice(&bytes).ok()?;
    let now = Date::now().as_millis() as u64 / 1000;
    if !uuid(&claims.sub) || claims.exp <= now || claims.exp > now + 86400 { return None; }
    Some(claims.sub)
}

pub fn message_key(env: &Env) -> Result<[u8; 32], String> {
    let encoded = env.secret("MESSAGE_KEY").map_err(|_| "missing message key")?.to_string();
    let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| "bad message key")?;
    bytes.try_into().map_err(|_| "message key must be 32 bytes".into())
}

pub fn derive_message_key(env: &Env, chat: &str, purpose: &[u8]) -> Result<[u8; 32], String> {
    let mut master = message_key(env)?;
    let hkdf = hkdf::Hkdf::<Sha256>::new(Some(chat.as_bytes()), &master);
    let mut derived = [0u8; 32];
    let result = hkdf.expand(purpose, &mut derived);
    master.fill(0);
    result.map_err(|_| "HKDF expand failed".to_string())?;
    Ok(derived)
}
