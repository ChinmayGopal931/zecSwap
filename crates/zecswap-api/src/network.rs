use std::fmt;
use std::str::FromStr;

use alloy_primitives::{Address, address};
use serde::{Deserialize, Serialize};
use zcash_protocol::consensus::{self, NetworkType, NetworkUpgrade, Parameters};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Testnet,
    Mainnet,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Railgun {
    pub proxy: Address,
    pub relay_adapt: Address,
}

struct Chain {
    id: u64,
    railgun: OnChain,
    token: Option<Address>,
}

enum OnChain {
    Railgun(Railgun),
    None,
    Any,
}

const TESTNET: &[Chain] = &[
    Chain {
        id: 11_155_111,
        railgun: OnChain::Railgun(Railgun {
            proxy: address!("eCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea"),
            relay_adapt: address!("7e3d929EbD5bDC84d02Bd3205c777578f33A214D"),
        }),
        token: None,
    },
    Chain {
        id: 84_532,
        railgun: OnChain::None,
        token: None,
    },
    Chain {
        id: 31_337,
        railgun: OnChain::Any,
        token: None,
    },
];

const MAINNET: &[Chain] = &[Chain {
    id: 1,
    railgun: OnChain::Railgun(Railgun {
        proxy: address!("FA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9"),
        relay_adapt: address!("Ac9f360Ae85469B27aEDdEaFC579Ef2d052aD405"),
    }),
    token: Some(address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")),
}];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mismatch {
    Chain(Network, u64),
    Railgun(Network, Address),
    Token(Network, Address),
}

impl Network {
    pub fn name(self) -> &'static str {
        match self {
            Network::Testnet => "testnet",
            Network::Mainnet => "mainnet",
        }
    }

    pub fn zcash(self) -> consensus::Network {
        match self {
            Network::Testnet => consensus::Network::TestNetwork,
            Network::Mainnet => consensus::Network::MainNetwork,
        }
    }

    pub fn zcash_type(self) -> NetworkType {
        self.zcash().network_type()
    }

    pub fn ironwood(self) -> bool {
        self.zcash()
            .activation_height(NetworkUpgrade::Nu6_3)
            .is_some()
    }

    pub fn reveal_headroom_wei(self) -> u128 {
        match self {
            Network::Testnet => 0,
            Network::Mainnet => 5_000_000_000,
        }
    }

    pub fn lightwalletd(self) -> &'static str {
        match self {
            Network::Testnet => "https://testnet.zec.rocks:443",
            Network::Mainnet => "https://zec.rocks:443",
        }
    }

    pub fn check(self, chain_id: u64, railgun: Address, token: Address) -> Result<(), Mismatch> {
        let chain = self
            .chain(chain_id)
            .ok_or(Mismatch::Chain(self, chain_id))?;
        let ours = match chain.railgun {
            OnChain::Railgun(ours) => railgun == ours.proxy,
            OnChain::None => false,
            OnChain::Any => true,
        };
        if !railgun.is_zero() && !ours {
            return Err(Mismatch::Railgun(self, railgun));
        }
        if chain.token.is_some_and(|fixed| fixed != token) {
            return Err(Mismatch::Token(self, token));
        }
        Ok(())
    }

    pub fn relay_adapt(self, chain_id: u64) -> Option<Address> {
        match self.chain(chain_id)?.railgun {
            OnChain::Railgun(railgun) => Some(railgun.relay_adapt),
            OnChain::None | OnChain::Any => None,
        }
    }

    fn chain(self, chain_id: u64) -> Option<&'static Chain> {
        let chains = match self {
            Network::Testnet => TESTNET,
            Network::Mainnet => MAINNET,
        };
        chains.iter().find(|chain| chain.id == chain_id)
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Network {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name {
            "testnet" => Ok(Network::Testnet),
            "mainnet" => Ok(Network::Mainnet),
            _ => Err(format!("{name} is neither testnet nor mainnet")),
        }
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mismatch::Chain(network, chain_id) => {
                write!(
                    f,
                    "the EVM RPC is on chain {chain_id}, not a {network} chain"
                )
            }
            Mismatch::Railgun(network, railgun) => write!(
                f,
                "the contract pays into a Railgun at {railgun}, not {network}'s"
            ),
            Mismatch::Token(network, token) => {
                write!(
                    f,
                    "the token {token} is not the one {network} swaps pay out"
                )
            }
        }
    }
}

impl std::error::Error for Mismatch {}

#[cfg(test)]
mod tests {
    use zcash_protocol::consensus::{BlockHeight, BranchId};

    use super::*;

    const SEPOLIA_RAILGUN: Address = address!("eCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea");
    const MAINNET_RAILGUN: Address = address!("FA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    #[test]
    fn each_network_takes_its_own_chains_railgun_and_token_alone() {
        let any = Address::repeat_byte(7);
        assert_eq!(
            Network::Testnet.check(11_155_111, SEPOLIA_RAILGUN, any),
            Ok(())
        );
        assert_eq!(Network::Testnet.check(84_532, Address::ZERO, any), Ok(()));
        assert_eq!(Network::Testnet.check(31_337, any, any), Ok(()));
        assert_eq!(Network::Mainnet.check(1, MAINNET_RAILGUN, USDC), Ok(()));
        assert_eq!(Network::Mainnet.check(1, Address::ZERO, USDC), Ok(()));

        assert_eq!(
            Network::Testnet.check(1, MAINNET_RAILGUN, USDC),
            Err(Mismatch::Chain(Network::Testnet, 1))
        );
        assert_eq!(
            Network::Mainnet.check(11_155_111, SEPOLIA_RAILGUN, any),
            Err(Mismatch::Chain(Network::Mainnet, 11_155_111))
        );
        assert_eq!(
            Network::Mainnet.check(31_337, any, USDC),
            Err(Mismatch::Chain(Network::Mainnet, 31_337))
        );
        assert_eq!(
            Network::Testnet.check(11_155_111, MAINNET_RAILGUN, any),
            Err(Mismatch::Railgun(Network::Testnet, MAINNET_RAILGUN))
        );
        assert_eq!(
            Network::Testnet.check(84_532, SEPOLIA_RAILGUN, any),
            Err(Mismatch::Railgun(Network::Testnet, SEPOLIA_RAILGUN))
        );
        assert_eq!(
            Network::Mainnet.check(1, any, USDC),
            Err(Mismatch::Railgun(Network::Mainnet, any))
        );
        assert_eq!(
            Network::Mainnet.check(1, MAINNET_RAILGUN, any),
            Err(Mismatch::Token(Network::Mainnet, any))
        );
    }

    #[test]
    fn relay_adapt_is_railguns_own_where_the_network_has_railgun() {
        assert_eq!(
            Network::Testnet.relay_adapt(11_155_111),
            Some(address!("7e3d929EbD5bDC84d02Bd3205c777578f33A214D"))
        );
        assert_eq!(
            Network::Mainnet.relay_adapt(1),
            Some(address!("Ac9f360Ae85469B27aEDdEaFC579Ef2d052aD405"))
        );
        assert_eq!(Network::Testnet.relay_adapt(84_532), None);
        assert_eq!(Network::Testnet.relay_adapt(31_337), None);
        assert_eq!(Network::Mainnet.relay_adapt(11_155_111), None);
    }

    #[test]
    fn names_read_back_and_ironwood_is_scheduled_on_both() {
        for network in [Network::Testnet, Network::Mainnet] {
            assert_eq!(network.name().parse(), Ok(network));
            assert!(network.ironwood());
        }
        assert!("main".parse::<Network>().is_err());
    }

    #[test]
    fn nu7_activates_at_each_networks_height() {
        for (network, height) in [(Network::Mainnet, 3_543_405), (Network::Testnet, 4_465_026)] {
            let zcash = network.zcash();
            let activation = BlockHeight::from_u32(height);
            assert_eq!(
                zcash.activation_height(NetworkUpgrade::Nu7),
                Some(activation)
            );
            assert_eq!(
                BranchId::for_height(&zcash, activation - 1),
                BranchId::Nu6_3
            );
            assert_eq!(BranchId::for_height(&zcash, activation), BranchId::Nu7);
        }
    }
}
