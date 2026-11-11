//! Authentication and purpose-separated key derivation.
//! All session tokens require a live server-side D1 session on every API request.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use worker::{Date, Env};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub sid: String,
    pub aud: String,
    pub exp: u64,
}

pub fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

pub fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "secure random source failed")?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(uuid::Uuid::from_bytes(bytes).to_string())
}

pub fn now() -> u64 {
    Date::now().as_millis() / 1000
}
pub fn now_ms() -> i64 {
    Date::now().as_millis() as i64
}

pub fn admin(env: &Env, provided: Option<String>) -> bool {
    let Ok(key) = env.secret("ADMIN_KEY") else {
        return false;
    };
    let Some(provided) = provided else {
        return false;
    };
    let key = key.to_string();
    key.len() >= 32 && bool::from(key.as_bytes().ct_eq(provided.as_bytes()))
}

fn decode_key(env: &Env, name: &str) -> Result<[u8; 32], String> {
    let encoded = env
        .secret(name)
        .map_err(|_| format!("missing secret {name}"))?
        .to_string();
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| format!("invalid secret {name}"))?;
    decoded
        .try_into()
        .map_err(|_| format!("{name} must be 32 raw bytes, base64url"))
}

pub fn version(env: &Env, var: &str) -> Result<u8, String> {
    let raw = env
        .var(var)
        .map(|x| x.to_string())
        .unwrap_or_else(|_| "1".into());
    match raw.as_str() {
        "1" => Ok(1),
        "2" => Ok(2),
        _ => Err(format!("invalid {var}")),
    }
}

fn auth_key(env: &Env, version: u8) -> Result<[u8; 32], String> {
    if !(1..=2).contains(&version) {
        return Err("invalid auth key version".into());
    }
    decode_key(env, &format!("AUTH_KEY_V{version}"))
}

pub fn message_key(env: &Env, version: u8) -> Result<[u8; 32], String> {
    if !(1..=2).contains(&version) {
        return Err("invalid message key version".into());
    }
    decode_key(env, &format!("MESSAGE_KEY_V{version}"))
}

pub fn derive_message_key(env: &Env, chat: &str, version: u8) -> Result<[u8; 32], String> {
    let mut master = message_key(env, version)?;
    let hkdf = Hkdf::<Sha256>::new(Some(chat.as_bytes()), &master);
    let mut derived = [0u8; 32];
    let result = hkdf.expand(b"api-chat/aes256gcm/v2", &mut derived);
    master.fill(0);
    result.map_err(|_| "HKDF expand failed".to_string())?;
    Ok(derived)
}

pub fn fingerprint(
    env: &Env,
    chat: &str,
    sender: &str,
    client_id: &str,
    text: &str,
) -> Result<String, String> {
    let mut key = decode_key(env, "DEDUP_KEY")?;
    let mut h =
        <HmacSha256 as hmac::digest::KeyInit>::new_from_slice(&key).map_err(|_| "HMAC init")?;
    key.fill(0);
    for chunk in [
        b"api-chat/fingerprint/v2".as_slice(),
        chat.as_bytes(),
        sender.as_bytes(),
        client_id.as_bytes(),
        text.as_bytes(),
    ] {
        h.update(&(chunk.len() as u64).to_be_bytes());
        h.update(chunk);
    }
    Ok(URL_SAFE_NO_PAD.encode(h.finalize().into_bytes()))
}

fn sign(key: &[u8; 32], data: &str) -> Result<[u8; 32], String> {
    let mut h =
        <HmacSha256 as hmac::digest::KeyInit>::new_from_slice(key).map_err(|_| "HMAC init")?;
    h.update(data.as_bytes());
    Ok(h.finalize().into_bytes().into())
}

pub fn issue(
    env: &Env,
    user: &str,
    session: &str,
    ttl: u64,
    audience: &str,
) -> Result<String, String> {
    let max_ttl = match audience {
        "access" => 3600,
        "ws" => 120,
        _ => return Err("invalid audience".into()),
    };
    if !uuid(user) || !uuid(session) || ttl == 0 || ttl > max_ttl {
        return Err("invalid token parameters".into());
    }
    let claims = Claims {
        sub: user.into(),
        sid: session.into(),
        aud: audience.into(),
        exp: now() + ttl,
    };
    let payload = serde_json::to_vec(&claims).map_err(|_| "serialization failed")?;
    let v = version(env, "ACTIVE_AUTH_KEY_VERSION")?;
    let encoded = URL_SAFE_NO_PAD.encode(payload);
    let signed = format!("{v}.{encoded}");
    let signature = sign(&auth_key(env, v)?, &signed)?;
    Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature)))
}

pub fn verify(env: &Env, token: &str, audience: &str) -> Option<Claims> {
    if token.len() > 2048 {
        return None;
    }
    let mut parts = token.split('.');
    let version_str = parts.next()?;
    let payload = parts.next()?;
    let signature = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let v: u8 = version_str.parse().ok()?;
    let supplied = URL_SAFE_NO_PAD.decode(signature).ok()?;
    if supplied.len() != 32 {
        return None;
    }
    let key = auth_key(env, v).ok()?;
    let mut h = <HmacSha256 as hmac::digest::KeyInit>::new_from_slice(&key).ok()?;
    h.update(format!("{v}.{payload}").as_bytes());
    if h.verify_slice(&supplied).is_err() {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: Claims = serde_json::from_slice(&payload).ok()?;
    if !uuid(&claims.sub)
        || !uuid(&claims.sid)
        || claims.aud != audience
        || claims.exp <= now()
        || claims.exp > now() + 3600
    {
        return None;
    }
    Some(claims)
}

pub fn bearer(header: Option<String>) -> Option<String> {
    let header = header?;
    Some(header.strip_prefix("Bearer ")?.to_string())
}

pub fn cookie(header: Option<String>, name: &str) -> Option<String> {
    header?.split(';').find_map(|item| {
        let (k, v) = item.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_ids_only() {
        assert!(uuid("123e4567-e89b-42d3-a456-426614174000"));
        assert!(!uuid("123E4567-E89B-42D3-A456-426614174000"));
        assert!(!uuid("not-a-uuid"));
    }
    #[test]
    fn signatures_are_bound_to_payload() {
        let key = [7u8; 32];
        let data = "1.ZGF0YQ";
        let signature = sign(&key, data).unwrap();
        let mut h = <HmacSha256 as hmac::digest::KeyInit>::new_from_slice(&key).unwrap();
        h.update(data.as_bytes());
        assert!(h.verify_slice(&signature).is_ok());
        let mut wrong = <HmacSha256 as hmac::digest::KeyInit>::new_from_slice(&key).unwrap();
        wrong.update(b"2.ZGF0YQ");
        assert!(wrong.verify_slice(&signature).is_err());
    }
}
