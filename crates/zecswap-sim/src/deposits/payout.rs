use alloy::primitives::{Address, B256, U256, address};
use alloy::sol;
use alloy::sol_types::{SolCall, SolEvent};
use zecswap_chain::evm::{reverse_funding_calls, reverse_swap_id, swap_id};
use zecswap_core::{Domain, NetworkType, ReverseOpen, Terms, derive_maker_share, derive_user_keys};
use zecswap_railgun::{Keys as RailgunKeys, ShieldNote};

use crate::node::Node;
use crate::relayer::FEE;
use crate::seed::Seed;
use crate::world::{
    CHAIN_ID, Chain, CiphertextWords, EPOCH, MIN_WINDOW, UNIT, claimCall, openCall, payoutCall,
    readyCall, refundCall, refundPayoutCall, terms_words,
};

pub const SWAP: u32 = 0;
const MAKER: Address = address!("3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a");
const FUNDER: Address = address!("f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0");
const AMOUNT: u128 = 4 * UNIT;

sol! {
    event Rescued(bytes32 indexed id, address relayer, uint256 fee);
    function vaultOf(bytes32 id) external view returns (address);
    function rescueNonces(bytes32 id) external view returns (uint64);
}

#[derive(Clone, Debug)]
pub struct Paid {
    pub id: B256,
    pub terms: Terms,
    pub note: ShieldNote,
    pub vault: Address,
    pub reverse: bool,
}

pub fn settle(
    chain: &Chain,
    seed: Seed,
    user_seed: &[u8; 64],
    relayer: Address,
    reverse: bool,
) -> Paid {
    let (node, contract) = (&chain.node, chain.contract);
    let keys = derive_user_keys(user_seed, NetworkType::Test, 0, SWAP).expect("the user's keys");
    let e = derive_maker_share(&seed.bytes("payout-maker-root"), 0).expect("a maker share");
    let z = U256::from_be_bytes(keys.share.to_be_bytes());
    let note = RailgunKeys::from_seed(user_seed, 0)
        .note(&keys.note_entropy)
        .expect("the user's note");
    let domain = Domain {
        chain_id: CHAIN_ID,
        contract: contract.into(),
    };
    let (t0, t1) = (EPOCH + 2 * MIN_WINDOW, EPOCH + 4 * MIN_WINDOW);
    let user = Address::from(keys.auth.address());
    let ciphertext = CiphertextWords {
        encryptedBundle: note.ciphertext.encrypted_bundle.map(B256::from),
        shieldKey: note.ciphertext.shield_key.into(),
    };
    let call = |from: Address, to: Address, data: Vec<u8>| {
        node.transact(from, Some(to), data.into());
    };
    let (id, terms) = match reverse {
        false => {
            let terms = Terms {
                maker: MAKER.into(),
                token: chain.token.into(),
                amount: AMOUNT,
                maker_share: e.public(),
                user_share: keys.share.public(),
                user: user.into(),
                t0,
                t1,
                payout_note: note.commitment(),
            };
            let words = terms_words(&terms);
            let id = swap_id(MAKER, &terms.user_share);
            chain.stock(MAKER, AMOUNT);
            let opening = openCall {
                token: chain.token,
                amount: AMOUNT,
                makerKey: words.makerKey,
                userKey: words.userKey,
                user,
                t0,
                t1,
                payoutNote: words.payoutNote,
            };
            call(MAKER, contract, opening.abi_encode());
            let readying = readyCall {
                id,
                terms: words.clone(),
            };
            call(MAKER, contract, readying.abi_encode());
            let claiming = claimCall {
                id,
                terms: words.clone(),
                userSecret: z,
            };
            call(MAKER, contract, claiming.abi_encode());
            let signature = keys.auth.sign(&domain.payout(&id.0, &relayer.into(), FEE));
            let paying = payoutCall {
                id,
                terms: words,
                npk: note.npk.into(),
                ciphertext,
                fee: FEE,
                signature: signature.to_vec().into(),
            };
            call(relayer, contract, paying.abi_encode());
            (id, terms)
        }
        true => {
            let open = ReverseOpen {
                maker: MAKER.into(),
                user: user.into(),
                token: chain.token.into(),
                amount: AMOUNT,
                maker_share: e.public(),
                user_share: keys.share.public(),
                t0,
                t1,
                refund_note: note.commitment(),
                deadline: EPOCH + MIN_WINDOW,
            };
            let signature = keys.auth.sign(&domain.open_reverse(&open));
            let terms = open.terms();
            let words = terms_words(&terms);
            let id = reverse_swap_id(user, &open.maker_share);
            chain.mint(FUNDER, AMOUNT);
            for (to, data) in reverse_funding_calls(contract, &open, &signature) {
                call(FUNDER, to, data);
            }
            let refunding = refundCall {
                id,
                terms: words.clone(),
                makerSecret: z,
            };
            call(FUNDER, contract, refunding.abi_encode());
            let signature = keys
                .auth
                .sign(&domain.refund_payout(&id.0, &relayer.into(), FEE));
            let paying = refundPayoutCall {
                id,
                terms: words,
                npk: note.npk.into(),
                ciphertext,
                fee: FEE,
                signature: signature.to_vec().into(),
            };
            call(relayer, contract, paying.abi_encode());
            (id, terms)
        }
    };
    let out = node.view(contract, vaultOfCall { id }.abi_encode().into());
    let vault = vaultOfCall::abi_decode_returns(&out.expect("the contract answers vaultOf"))
        .expect("a vault's address");
    Paid {
        id,
        terms,
        note,
        vault,
        reverse,
    }
}

pub fn rescues(node: &Node, contract: Address, id: B256) -> Vec<(B256, Address, u128)> {
    let logs = node.logs().into_iter();
    logs.filter(|logged| logged.log.address == contract)
        .filter_map(|logged| {
            let event = Rescued::decode_log(&logged.log).ok()?;
            (event.id == id).then(|| (logged.tx, event.relayer, event.fee.saturating_to()))
        })
        .collect()
}

pub fn nonce(node: &Node, contract: Address, id: B256) -> u64 {
    let out = node.view(contract, rescueNoncesCall { id }.abi_encode().into());
    rescueNoncesCall::abi_decode_returns(&out.expect("the contract answers rescueNonces"))
        .expect("a nonce")
}
