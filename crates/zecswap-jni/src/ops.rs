use rand::{rand_core::UnwrapErr, rngs::SysRng};
use zecswap_core::{
    Domain, JointAccount, NetworkType, Payout, PublicShare, ReverseOpen, SecretShare, ShareProof,
    SwapContext, SweepIntent, UserSwapKeys, derive_deposit_keys, derive_user_keys, sign_pczt_bytes,
};
use zecswap_railgun::{Keys as RailgunKeys, ShieldNote};

pub const SEED_BYTES: usize = 64;
const RAILGUN_WALLET: u32 = 0;

pub type Result<T> = core::result::Result<T, String>;

pub struct ReverseTerms {
    pub maker: [u8; 20],
    pub token: [u8; 20],
    pub amount: u128,
    pub maker_share: [u8; 64],
    pub ready_deadline: u64,
    pub refund_after: u64,
    pub funding_deadline: u64,
}

pub enum ReverseAction {
    Ready,
    LockRefund,
}

pub struct Swap<'a> {
    seed: &'a [u8],
    network: NetworkType,
    index: u32,
}

impl<'a> Swap<'a> {
    pub fn sign_reverse_open(
        &self,
        railgun_seed: &[u8],
        domain: Domain,
        terms: &ReverseTerms,
    ) -> Result<[u8; 65]> {
        if terms.amount == 0
            || terms.maker == [0; 20]
            || terms.token == [0; 20]
            || terms.funding_deadline >= terms.ready_deadline
            || terms.ready_deadline >= terms.refund_after
        {
            return Err("invalid reverse escrow terms".into());
        }
        let keys = self.keys()?;
        let open = ReverseOpen {
            maker: terms.maker,
            user: keys.auth.address(),
            token: terms.token,
            amount: terms.amount,
            maker_share: PublicShare::from_affine_bytes(&terms.maker_share).map_err(error)?,
            user_share: keys.share.public(),
            t0: terms.ready_deadline,
            t1: terms.refund_after,
            refund_note: self.note(&keys, railgun_seed)?.commitment(),
            deadline: terms.funding_deadline,
        };
        Ok(keys.auth.sign(&domain.open_reverse(&open)))
    }

    pub fn sign_reverse_action(
        &self,
        domain: Domain,
        swap_id: &[u8; 32],
        deadline: u64,
        action: ReverseAction,
    ) -> Result<[u8; 65]> {
        let digest = match action {
            ReverseAction::Ready => domain.ready(swap_id, deadline),
            ReverseAction::LockRefund => domain.lock_refund(swap_id, deadline),
        };
        Ok(self.keys()?.auth.sign(&digest))
    }

    pub fn sign_refund_payout(
        &self,
        domain: Domain,
        swap_id: &[u8; 32],
        relayer: &[u8; 20],
        fee: u128,
    ) -> Result<[u8; 65]> {
        Ok(self
            .keys()?
            .auth
            .sign(&domain.refund_payout(swap_id, relayer, fee)))
    }

    pub fn sign_refund_rescue(
        &self,
        railgun_seed: &[u8],
        domain: Domain,
        swap_id: &[u8; 32],
        relayer: &[u8; 20],
        fee: u128,
        authorization: zecswap_core::RescueAuthorization,
    ) -> Result<[u8; 65]> {
        let keys = self.keys()?;
        let note = self.note(&keys, railgun_seed)?;
        Ok(keys
            .auth
            .sign(&domain.rescue(swap_id, &note.commitment(), relayer, fee, authorization)))
    }

    pub fn new(seed: &'a [u8], mainnet: bool, index: i32) -> Result<Self> {
        check_seed(seed)?;
        let index = u32::try_from(index).map_err(|_| format!("a swap index can't be {index}"))?;
        Ok(Self {
            seed,
            network: network(mainnet),
            index,
        })
    }

    pub fn user_share(&self) -> Result<[u8; 64]> {
        Ok(self.keys()?.share.public().to_affine_bytes())
    }

    pub fn auth_address(&self) -> Result<[u8; 20]> {
        Ok(self.keys()?.auth.address())
    }

    pub fn claim_secret(&self) -> Result<[u8; 32]> {
        Ok(self.keys()?.share.to_be_bytes())
    }

    pub fn payout_note(&self, railgun_seed: &[u8]) -> Result<Vec<u8>> {
        Ok(note_bytes(&self.note(&self.keys()?, railgun_seed)?))
    }

    pub fn accept(
        &self,
        railgun_seed: &[u8],
        context: SwapContext,
        maker_share: &[u8; 64],
        maker_proof: &[u8; 64],
    ) -> Result<Vec<u8>> {
        let keys = self.keys()?;
        let maker = PublicShare::from_affine_bytes(maker_share).map_err(error)?;
        context
            .verify_maker(&maker, &ShareProof::from_bytes(*maker_proof))
            .map_err(|e| format!("the maker's share proof: {e}"))?;
        let payout = Payout {
            user: keys.auth.address(),
            note: Some(self.note(&keys, railgun_seed)?.commitment()),
        };
        let proof = context.prove_user(&maker, &keys.share, &payout, UnwrapErr(SysRng));
        let mut bytes = Vec::with_capacity(3 * 64);
        bytes.extend_from_slice(&keys.share.public().to_affine_bytes());
        bytes.extend_from_slice(&proof.to_bytes());
        bytes.extend_from_slice(&keys.viewing.to_bytes());
        Ok(bytes)
    }

    pub fn deposit_account(&self, maker_share: &[u8; 64]) -> Result<[String; 2]> {
        let joint = self.joint(&self.keys()?, maker_share)?;
        Ok([
            joint.unified_address(self.network),
            joint.ufvk(self.network),
        ])
    }

    pub fn sign_lock_claim(
        &self,
        domain: Domain,
        swap_id: &[u8; 32],
        deadline: u64,
    ) -> Result<[u8; 65]> {
        Ok(self
            .keys()?
            .auth
            .sign(&domain.lock_claim(swap_id, deadline)))
    }

    pub fn sign_payout(
        &self,
        domain: Domain,
        swap_id: &[u8; 32],
        relayer: &[u8; 20],
        fee: u128,
    ) -> Result<[u8; 65]> {
        Ok(self
            .keys()?
            .auth
            .sign(&domain.payout(swap_id, relayer, fee)))
    }

    pub fn sign_refund(
        &self,
        maker_share: &[u8; 64],
        maker_secret: &[u8; 32],
        pczt: &[u8],
        intent: &SweepIntent,
    ) -> Result<Vec<u8>> {
        let keys = self.keys()?;
        let joint = self.joint(&keys, maker_share)?;
        let e = SecretShare::from_be_bytes(maker_secret).map_err(error)?;
        let key = joint.spend_key(&e, &keys.share).map_err(error)?;
        sign_pczt_bytes(pczt, &[key], intent).map_err(error)
    }

    fn keys(&self) -> Result<UserSwapKeys> {
        derive_user_keys(self.seed, self.network, 0, self.index).map_err(error)
    }

    fn note(&self, keys: &UserSwapKeys, railgun_seed: &[u8]) -> Result<ShieldNote> {
        railgun(railgun_seed)?
            .note(&keys.note_entropy)
            .map_err(error)
    }

    fn joint(&self, keys: &UserSwapKeys, maker_share: &[u8; 64]) -> Result<JointAccount> {
        let maker = PublicShare::from_affine_bytes(maker_share).map_err(error)?;
        JointAccount::derive(&maker, &keys.share.public(), &keys.viewing).map_err(error)
    }
}

pub fn deposit_note(
    seed: &[u8],
    mainnet: bool,
    account: u32,
    index: u32,
    railgun_seed: &[u8],
    rescue: bool,
    number: u64,
) -> Result<Vec<u8>> {
    check_seed(seed)?;
    let keys = derive_deposit_keys(seed, network(mainnet), account, index).map_err(error)?;
    let entropy = if rescue {
        keys.rescue_entropy(number)
    } else {
        keys.shield_entropy(number)
    };
    let note = railgun(railgun_seed)?.note(&entropy).map_err(error)?;
    Ok(note_bytes(&note))
}

pub fn check_open_fee(mainnet: bool, address: &str, zat: u64, max_zat: u64) -> Result<()> {
    if zat == 0 || zat > max_zat {
        return Err(format!(
            "the maker asks an open fee of {zat} zat, outside the {max_zat} allowed"
        ));
    }
    zecswap_core::orchard_only(address, network(mainnet))
        .map(drop)
        .map_err(|wrong| format!("the open fee's address {wrong}"))
}

pub fn railgun_address(seed: &[u8]) -> Result<String> {
    Ok(railgun(seed)?.address())
}

fn network(mainnet: bool) -> NetworkType {
    if mainnet {
        NetworkType::Main
    } else {
        NetworkType::Test
    }
}

fn note_bytes(note: &ShieldNote) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(6 * 32);
    note.words()
        .iter()
        .for_each(|word| bytes.extend_from_slice(word));
    bytes.extend_from_slice(&note.commitment());
    bytes
}

fn railgun(seed: &[u8]) -> Result<RailgunKeys> {
    check_seed(seed)?;
    Ok(RailgunKeys::from_seed(seed, RAILGUN_WALLET))
}

fn check_seed(seed: &[u8]) -> Result<()> {
    match seed.len() {
        SEED_BYTES => Ok(()),
        length => Err(format!("the seed must be {SEED_BYTES} bytes, not {length}")),
    }
}

fn error(e: impl core::fmt::Display) -> String {
    e.to_string()
}
