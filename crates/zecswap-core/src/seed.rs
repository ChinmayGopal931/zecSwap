use blake2b_simd::Params;
use ff::FromUniformBytes;
use pasta_curves::pallas;
use zcash_protocol::consensus::NetworkType;

use zeroize::Zeroizing;

use crate::{AuthKey, Error, SecretShare, ViewingKeys};

const ROOT_PERSONALIZATION: &[u8; 16] = b"ZecSwap_UserRoot";
const EXPAND_PERSONALIZATION: &[u8; 16] = b"ZecSwap_UserKeys";
const MAKER_PERSONALIZATION: &[u8; 16] = b"ZecSwap_MakerKey";

pub struct UserSwapKeys {
    pub share: SecretShare,
    pub viewing: ViewingKeys,
    pub auth: AuthKey,
    pub note_entropy: Zeroizing<[u8; 32]>,
}

pub fn derive_user_keys(
    seed: &[u8],
    network: NetworkType,
    account: u32,
    index: u32,
) -> Result<UserSwapKeys, Error> {
    let coin_type: u32 = match network {
        NetworkType::Main => 133,
        NetworkType::Test | NetworkType::Regtest => 1,
    };
    let root = Params::new()
        .hash_length(32)
        .personal(ROOT_PERSONALIZATION)
        .to_state()
        .update(seed)
        .update(&coin_type.to_le_bytes())
        .update(&account.to_le_bytes())
        .finalize();
    let expand = |domain: u8| {
        *Params::new()
            .hash_length(64)
            .personal(EXPAND_PERSONALIZATION)
            .to_state()
            .update(root.as_bytes())
            .update(&[domain])
            .update(&index.to_le_bytes())
            .finalize()
            .as_array()
    };

    let mut note_entropy = Zeroizing::new([0; 32]);
    note_entropy.copy_from_slice(&expand(4)[..32]);
    Ok(UserSwapKeys {
        share: SecretShare::from_scalar(pallas::Scalar::from_uniform_bytes(&expand(0)))?,
        viewing: ViewingKeys::from_uniform(&expand(1), &expand(2)),
        auth: AuthKey::from_uniform(&expand(3))?,
        note_entropy,
    })
}

pub fn derive_maker_share(root: &[u8; 32], nonce: u64) -> Result<SecretShare, Error> {
    let hash = Params::new()
        .hash_length(64)
        .personal(MAKER_PERSONALIZATION)
        .to_state()
        .update(root)
        .update(&nonce.to_le_bytes())
        .finalize();
    SecretShare::from_scalar(pallas::Scalar::from_uniform_bytes(hash.as_array()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 64] = [42; 64];

    #[test]
    fn index_account_network_and_seed_separate_keys() {
        let base = derive_user_keys(&SEED, NetworkType::Main, 0, 0)
            .unwrap()
            .share
            .public();
        let base_auth = derive_user_keys(&SEED, NetworkType::Main, 0, 0)
            .unwrap()
            .auth
            .address();
        for other in [
            derive_user_keys(&SEED, NetworkType::Main, 0, 1),
            derive_user_keys(&SEED, NetworkType::Main, 1, 0),
            derive_user_keys(&SEED, NetworkType::Test, 0, 0),
            derive_user_keys(&[43; 64], NetworkType::Main, 0, 0),
        ] {
            let other = other.unwrap();
            assert_ne!(other.share.public(), base);
            assert_ne!(other.auth.address(), base_auth);
        }
    }
}
