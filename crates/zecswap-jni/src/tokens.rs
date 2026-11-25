use zecswap_tokens::{Challenge, Pending, Token, TokenKey, read_www_authenticate};
use zeroize::Zeroizing;

use crate::ops::Result;

pub fn read_challenge(header: &str) -> Result<[Vec<u8>; 3]> {
    let (challenge, key) = read_www_authenticate(header).map_err(error)?;
    Ok([
        challenge.encode(),
        key.spki().to_vec(),
        challenge.issuer().as_bytes().to_vec(),
    ])
}

pub fn blind(token_key: &[u8], challenge: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    let key = TokenKey::from_spki(token_key).map_err(error)?;
    let challenge = Challenge::decode(challenge).map_err(error)?;
    let (pending, blinded) = Pending::new(&key, &challenge).map_err(error)?;
    let pending = Zeroizing::new(pending.to_bytes());
    Ok(Zeroizing::new([&blinded[..], &pending[..]].concat()))
}

pub fn finalize(pending: &[u8], token_key: &[u8], blind_signature: &[u8]) -> Result<Vec<u8>> {
    let key = TokenKey::from_spki(token_key).map_err(error)?;
    let token = Pending::from_bytes(pending)
        .map_err(error)?
        .finalize(&key, blind_signature)
        .map_err(error)?;
    Ok(token.encode())
}

pub fn authorization(token: &[u8]) -> Result<String> {
    Ok(Token::decode(token).map_err(error)?.authorization())
}

fn error(e: impl core::fmt::Display) -> String {
    e.to_string()
}
