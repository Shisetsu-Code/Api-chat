//! Versioned, authenticated encryption. The server is able to decrypt (not E2EE).
use aes_gcm::{Aes256Gcm, Nonce, aead::{Aead, KeyInit, Payload}};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use worker::{Env, Error, Result};
use crate::security;

fn failure(msg: &str) -> Error { Error::RustError(msg.into()) }

fn cipher(env:&Env, chat:&str, version:u8) -> Result<Aes256Gcm> {
    let mut key=security::derive_message_key(env,chat,version)
        .map_err(|_| failure("message key unavailable"))?;
    let cipher=Aes256Gcm::new_from_slice(&key).map_err(|_|failure("cipher initialization"))?;
    key.fill(0);
    Ok(cipher)
}
fn aad(chat:&str, seq:u64, version:u8) -> String {
    format!("api-chat/v2/{chat}/{seq}/{version}")
}

pub fn seal(env:&Env,chat:&str,seq:u64,version:u8,text:&str)->Result<(String,String)> {
    let cipher=cipher(env,chat,version)?;
    let mut nonce=[0u8;12];
    getrandom::fill(&mut nonce).map_err(|_|failure("secure random unavailable"))?;
    let associated=aad(chat,seq,version);
    let ct=cipher.encrypt(Nonce::from_slice(&nonce),
        Payload{msg:text.as_bytes(),aad:associated.as_bytes()})
        .map_err(|_|failure("AES-GCM encrypt failed"))?;
    Ok((URL_SAFE_NO_PAD.encode(nonce),URL_SAFE_NO_PAD.encode(ct)))
}
pub fn open(env:&Env,chat:&str,seq:u64,version:u8,nonce:&str,ciphertext:&str)->Result<String> {
    let cipher=cipher(env,chat,version)?;
    let nonce=URL_SAFE_NO_PAD.decode(nonce).map_err(|_|failure("bad nonce encoding"))?;
    if nonce.len()!=12{return Err(failure("bad nonce length"));}
    let ct=URL_SAFE_NO_PAD.decode(ciphertext).map_err(|_|failure("bad ciphertext"))?;
    let associated=aad(chat,seq,version);
    let bytes=cipher.decrypt(Nonce::from_slice(&nonce),
        Payload{msg:&ct,aad:associated.as_bytes()})
        .map_err(|_|failure("AES-GCM authentication failed"))?;
    String::from_utf8(bytes).map_err(|_|failure("invalid utf8 in message"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn key_rotation_binds_aad_to_key_version() {
        assert_ne!(aad("chat",12,1),aad("chat",12,2));
        assert_ne!(aad("chat",12,1),aad("chat",13,1));
    }
}
