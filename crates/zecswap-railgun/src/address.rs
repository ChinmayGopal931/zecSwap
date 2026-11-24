use bech32::{Bech32m, Hrp};

use crate::keys::Receiver;

const VERSION: u8 = 1;
const ALL_CHAINS: [u8; 8] = {
    let mut id = [0xff; 8];
    let railgun = b"railgun";
    let mut i = 0;
    while i < railgun.len() {
        id[i] ^= railgun[i];
        i += 1;
    }
    id
};

pub(crate) fn encode(receiver: &Receiver) -> String {
    let mut data = Vec::with_capacity(73);
    data.push(VERSION);
    data.extend_from_slice(&receiver.master_public_key);
    data.extend_from_slice(&ALL_CHAINS);
    data.extend_from_slice(&receiver.viewing_public_key);
    bech32::encode::<Bech32m>(Hrp::parse_unchecked("0zk"), &data)
        .expect("a 73-byte address is within bech32m's length limit")
}
