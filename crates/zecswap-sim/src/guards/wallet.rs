use alloy::primitives::aliases::U120;
use alloy::primitives::{Address, B256, Bytes, FixedBytes, U256, keccak256};
use alloy::sol_types::SolCall;
use zecswap_chain::evm::railgun::{
    Ciphertext, IRailgunSmartWallet, IRelayAdapt, Preimage, ShieldRequest, Transaction,
};
use zecswap_chain::evm::{IErc20, IZecSwap, reverse_funding_calls, reverse_swap_id};
use zecswap_core::{
    Domain, NetworkType, ReverseOpen, UserSwapKeys, derive_maker_share, derive_user_keys,
};
use zecswap_railgun::{Keys as RailgunKeys, Receiver, ShieldNote};

use super::railgun::{Circuit, Coin, Draft, Exit, Out, Reads, adapt_params, erc20, root};
use super::{Ask, Funded, Must, Post, Relayed, Request};
use crate::seed::Seed;
use crate::world::{CHAIN_ID, EPOCH, MIN_WINDOW, UNIT};

pub const COINS: usize = 17;
pub const COIN: u128 = 1_000_000_000;
pub const FLOOR: u128 = 100_000;
pub const FUND_FEE: u128 = 100_000;
pub const AMOUNT: u128 = 4 * UNIT;
const SENT: u128 = 10_000_000;
const MIN_GAS: u128 = 1_000_000_000;

pub struct Ground {
    pub seed: Seed,
    pub contract: Address,
    pub other_contract: Address,
    pub token: Address,
    pub other_token: Address,
    pub railgun: Address,
    pub adapt: Address,
    pub other_adapt: Address,
    pub relayer: Address,
    pub relayer_receiver: Receiver,
    pub maker: Address,
    pub user_seed: [u8; 64],
    pub own: Receiver,
    pub other: Receiver,
    pub cap: u128,
    pub honest_fee: u128,
    pub circuit: Circuit,
    pub coins: Vec<Vec<Coin>>,
    pub odd: Vec<Coin>,
    pub spent: Vec<Coin>,
}

pub fn coins(seed: Seed, of: &str, count: usize, token: Address) -> Vec<Coin> {
    (0..count)
        .map(|at| Coin {
            nullifier: B256::from(seed.bytes::<32>(&format!("guard-coin-{of}-{at}"))),
            token,
            value: COIN,
        })
        .collect()
}

fn word(address: Address) -> B256 {
    address.into_word()
}

impl Ground {
    fn out(&self, to: Receiver, token: Address, value: u128) -> Out {
        Out {
            to,
            token,
            value,
            reads: Reads::Clean,
        }
    }

    fn draft(&self, spends: Vec<Coin>, outs: Vec<Out>, exit: Option<Exit>) -> Draft {
        Draft {
            spends,
            outs,
            exit,
            chain_id: CHAIN_ID,
            adapt: Address::ZERO,
            min_gas_price: MIN_GAS,
            root: root(),
            annotation: 0,
            memo: 0,
        }
    }

    fn private(&self, spends: Vec<Coin>, fee: u128) -> Draft {
        let held: u128 = spends.iter().map(|coin| coin.value).sum();
        let outs = vec![
            self.out(self.relayer_receiver, self.token, fee),
            self.out(self.other, self.token, SENT),
            self.out(self.own, self.token, held - fee - SENT),
        ];
        self.draft(spends, outs, None)
    }

    fn spread(&self, coins: &[Coin], outputs: usize, fee: u128) -> Vec<Draft> {
        let share = fee.div_ceil(coins.len() as u128);
        coins
            .iter()
            .map(|coin| {
                let mut outs = vec![self.out(self.relayer_receiver, self.token, share)];
                for _ in 2..outputs {
                    outs.push(self.out(self.other, self.token, 1));
                }
                let left = coin.value - share - (outputs.saturating_sub(2) as u128);
                outs.push(self.out(self.own, self.token, left));
                self.draft(vec![*coin], outs, None)
            })
            .collect()
    }

    fn proved(&self, tag: &str, drafts: &[Draft]) -> Vec<Transaction> {
        drafts
            .iter()
            .enumerate()
            .map(|(at, draft)| {
                let mut built = self.circuit.build(self.seed, &format!("{tag}-{at}"), draft);
                self.circuit.prove(&mut built, draft);
                built
            })
            .collect()
    }

    fn transact(transactions: Vec<Transaction>) -> Vec<u8> {
        IRailgunSmartWallet::transactCall {
            _transactions: transactions,
        }
        .abi_encode()
    }

    fn post(&self, data: Vec<u8>) -> Post {
        Post {
            chain_id: CHAIN_ID,
            to: self.railgun,
            value: 0,
            data: data.into(),
            swap: None,
        }
    }

    pub fn build(
        &self,
        number: usize,
        request: &Request,
        priced: bool,
        sponsors: bool,
    ) -> Vec<(Post, Must)> {
        match request.ask {
            Ask::Relayed(relayed) => self.relayed(number, relayed, priced),
            Ask::Funded(funded) => {
                let (post, must) = self.funded(number, funded, request.at);
                let must = match sponsors {
                    true => must,
                    false => Must::Refuse,
                };
                vec![(post, must)]
            }
        }
    }

    fn relayed(&self, number: usize, relayed: Relayed, priced: bool) -> Vec<(Post, Must)> {
        let tag = format!("guard-send-{number}");
        let coins = &self.coins[number];
        let fee = self.honest_fee;
        let mut draft = self.private(coins[..2].to_vec(), fee);
        let mut must = Must::Refuse;
        let mut drafts = None;
        let withdraw = |kind: u8, token: Address| {
            let mut spends = vec![coins[0]];
            let mut outs = vec![self.out(self.relayer_receiver, self.token, fee)];
            if token == self.token {
                outs.push(self.out(self.own, token, COIN - fee - SENT));
            } else {
                spends.push(self.odd[number]);
                outs.push(self.out(self.own, self.token, COIN - fee));
                outs.push(self.out(self.own, token, COIN - SENT));
            }
            let exit = Exit {
                to: word(Address::from(self.seed.bytes::<20>(&format!("{tag}-to")))),
                token,
                value: SENT,
                kind,
            };
            self.draft(spends, outs, Some(exit))
        };
        match relayed {
            Relayed::Private | Relayed::Twin | Relayed::Rival => must = Must::Serve,
            Relayed::Withdrawal => {
                must = Must::Serve;
                draft = withdraw(1, self.token);
            }
            Relayed::Batch => {
                must = Must::Serve;
                drafts = Some(self.spread(&coins[..2], 2, fee));
            }
            Relayed::Offset => drafts = Some(self.spread(&coins[..2], 2, fee)),
            Relayed::OtherChain
            | Relayed::OtherProxy
            | Relayed::WithValue
            | Relayed::Selector
            | Relayed::Trailing
            | Relayed::Truncated
            | Relayed::DirtyWord => {}
            Relayed::Padding => draft.memo = 40,
            Relayed::Oversized => {
                let mut wide = self.spread(&coins[..4], 13, fee);
                for draft in &mut wide {
                    draft.memo = 1024;
                }
                drafts = Some(wide);
            }
            Relayed::TooManyTransactions => drafts = Some(self.spread(&coins[..17], 2, fee)),
            Relayed::TooManyNullifiers => draft = self.private(coins[..14].to_vec(), fee),
            Relayed::TooManyCommitments => drafts = Some(self.spread(&coins[..1], 14, fee)),
            Relayed::LongAnnotation => draft.annotation = 63,
            Relayed::LongMemo => draft.memo = 1025,
            Relayed::ExtraCiphertext => {}
            Relayed::Empty => drafts = Some(Vec::new()),
            Relayed::NoNullifiers => draft.spends.clear(),
            Relayed::NoCommitments => draft.outs.clear(),
            Relayed::BoundChain => draft.chain_id = CHAIN_ID + 1,
            Relayed::BoundAdapt => draft.adapt = self.adapt,
            Relayed::Redirect => draft = withdraw(2, self.token),
            Relayed::OtherUnshield => draft = withdraw(1, self.other_token),
            Relayed::MinGasPrice => draft.min_gas_price = self.cap + 1,
            Relayed::FeeShort => {
                draft = self.private(coins[..2].to_vec(), FLOOR - 1);
            }
            Relayed::FeeToken => {
                let mut spends = coins[..2].to_vec();
                spends.push(self.odd[number]);
                let mut outs = vec![self.out(self.relayer_receiver, self.other_token, fee)];
                outs.push(self.out(self.own, self.other_token, COIN - fee));
                outs.push(self.out(self.own, self.token, 2 * COIN));
                draft = self.draft(spends, outs, None);
            }
            Relayed::FeeReceiver => draft.outs[0].to = self.other,
            Relayed::FeeGarbled => draft.outs[0].reads = Reads::Garbled,
            Relayed::FeeClaims => {
                draft = self.private(coins[..2].to_vec(), 1);
                draft.outs[0].reads = Reads::Claims(fee);
            }
            Relayed::FeeSecond => draft.outs.swap(0, 2),
            Relayed::FeeUnderGas => {
                draft = self.private(coins[..2].to_vec(), FLOOR);
                must = match priced {
                    true => Must::Fail,
                    false => Must::Serve,
                };
            }
            Relayed::Spent => {
                draft = self.private(self.spent[number..number + 1].to_vec(), fee);
                must = Must::Fail;
            }
            Relayed::Doubled => {
                draft = self.private(vec![coins[0], coins[0]], fee);
                must = Must::Fail;
            }
            Relayed::Forged => {
                let mut forged = coins[0];
                forged.value *= 2;
                draft = self.private(vec![forged], fee);
                must = Must::Fail;
            }
            Relayed::UnknownRoot => {
                draft.root = keccak256(b"elsewhere");
                must = Must::Fail;
            }
        }
        let drafts = drafts.unwrap_or_else(|| vec![draft]);
        let mut transactions = self.proved(&tag, &drafts);
        if relayed == Relayed::ExtraCiphertext {
            let bound = &mut transactions[0].boundParams;
            let extra = bound.commitmentCiphertext[0].clone();
            bound.commitmentCiphertext.push(extra);
            self.circuit.prove(&mut transactions[0], &drafts[0]);
        }
        let honest = Self::transact(transactions);
        let mut data = honest.clone();
        match relayed {
            Relayed::Selector => data[0] ^= 1,
            Relayed::Offset => data[68..132].rotate_left(32),
            Relayed::Twin => data = offset(&honest),
            Relayed::Padding => *data.last_mut().expect("calldata") = 1,
            Relayed::Trailing => data.extend_from_slice(&[0; 32]),
            Relayed::Truncated => data.truncate(data.len() - 32),
            Relayed::DirtyWord => data[516 + 30] = 1,
            _ => {}
        }
        let mut sent = self.post(data);
        match relayed {
            Relayed::OtherChain => sent.chain_id = CHAIN_ID + 1,
            Relayed::OtherProxy => sent.to = self.adapt,
            Relayed::WithValue => sent.value = 1,
            _ => {}
        }
        match relayed {
            Relayed::Twin => vec![(sent, Must::Refuse), (self.post(honest), Must::Serve)],
            Relayed::Rival => {
                let mut other = self.private(coins[..2].to_vec(), fee);
                other.outs[1].value += 1;
                other.outs[2].value -= 1;
                let rival = Self::transact(self.proved(&format!("{tag}-rival"), &[other]));
                vec![(sent, must), (self.post(rival), Must::Fail)]
            }
            _ => vec![(sent, must)],
        }
    }

    pub fn opened(&self, number: usize, at: u64) -> (UserSwapKeys, ShieldNote, ReverseOpen) {
        let keys = derive_user_keys(&self.user_seed, NetworkType::Test, 0, number as u32)
            .expect("the user's keys");
        let maker_share = derive_maker_share(&self.seed.bytes("guard-maker-root"), number as u64)
            .expect("a maker share");
        let note = RailgunKeys::from_seed(&self.user_seed, 0)
            .note(&keys.note_entropy)
            .expect("the user's note");
        let at = EPOCH + at;
        let t0 = at + 50 * 60 + MIN_WINDOW + 10 * 60;
        let open = ReverseOpen {
            maker: self.maker.into(),
            user: keys.auth.address(),
            token: self.token.into(),
            amount: AMOUNT,
            maker_share: maker_share.public(),
            user_share: keys.share.public(),
            t0,
            t1: t0 + 2 * MIN_WINDOW,
            refund_note: note.commitment(),
            deadline: at + 50 * 60,
        };
        (keys, note, open)
    }

    pub fn rival(&self, number: usize, at: u64) -> [(Address, Vec<u8>); 2] {
        let (keys, _, mut open) = self.opened(number, at);
        open.refund_note = keccak256(b"a rival escrow").0;
        open.amount -= u128::from(self.seed.chance("guard-rival-terms", number as u64, 500));
        let domain = Domain {
            chain_id: CHAIN_ID,
            contract: self.contract.into(),
        };
        reverse_funding_calls(
            self.contract,
            &open,
            &keys.auth.sign(&domain.open_reverse(&open)),
        )
    }

    fn funded(&self, number: usize, funded: Funded, at: u64) -> (Post, Must) {
        let tag = format!("guard-fund-{number}");
        let (keys, note, mut open) = self.opened(number, at);
        let user = Address::from(open.user);
        let maker_share = open.maker_share;
        if funded == Funded::Late {
            open.deadline = EPOCH + at - 60;
        }
        match funded {
            Funded::OtherMaker => open.maker = self.relayer.into(),
            Funded::TermsToken => open.token = self.other_token.into(),
            Funded::NoUser => open.user = [0; 20],
            Funded::NoAmount => open.amount = 0,
            Funded::NoNote => open.refund_note = [0; 32],
            Funded::Deadlines => open.deadline = open.t0,
            _ => {}
        }
        let contract = match funded {
            Funded::OtherContract => self.other_contract,
            _ => self.contract,
        };
        let domain = Domain {
            chain_id: CHAIN_ID,
            contract: self.contract.into(),
        };
        let signer = match funded {
            Funded::Signer => {
                derive_user_keys(&self.user_seed, NetworkType::Test, 0, number as u32 + 1000)
                    .expect("another key")
                    .auth
            }
            _ => keys.auth,
        };
        let signature = signer.sign(&domain.open_reverse(&open));
        let [(token_to, approve), (open_to, opening)] =
            reverse_funding_calls(contract, &open, &signature);
        let mut approve = IErc20::approveCall::abi_decode(&approve).expect("an approval");
        let mut opening = IZecSwap::openReverseCall::abi_decode(&opening).expect("an open");
        match funded {
            Funded::Spender => approve.spender = self.relayer,
            Funded::ApproveAmount => approve.amount += U256::from(1),
            Funded::MakerShare => opening.terms.makerKey[1] ^= U256::from(1),
            Funded::UserShare => opening.terms.userKey[1] ^= U256::from(1),
            Funded::ShortSignature => opening.signature = opening.signature[..64].to_vec().into(),
            _ => {}
        }
        let approve = approve.abi_encode();
        let fee = match funded {
            Funded::FeeShort => FUND_FEE - 1,
            _ => FUND_FEE,
        };
        let payee = match funded {
            Funded::FeeElsewhere => self.maker,
            _ => self.relayer,
        };
        let pay = IErc20::transferCall {
            to: payee,
            amount: U256::from(fee),
        }
        .abi_encode();
        let dust = |token: Address, value: u128, npk: B256| ShieldRequest {
            preimage: Preimage {
                npk,
                token: erc20(token),
                value: U120::from(value),
            },
            ciphertext: Ciphertext {
                encryptedBundle: note.ciphertext.encrypted_bundle.map(B256::from),
                shieldKey: note.ciphertext.shield_key.into(),
            },
        };
        let npk = B256::from(note.npk);
        let shields = match funded {
            Funded::NoShields => Vec::new(),
            Funded::Dust => vec![dust(self.token, 1, npk)],
            Funded::DustToken => vec![dust(self.other_token, 0, npk)],
            Funded::DustNpk => vec![dust(self.token, 0, B256::ZERO)],
            _ => vec![dust(self.token, 0, npk)],
        };
        let shield = IRelayAdapt::shieldCall {
            _shieldRequests: shields,
        }
        .abi_encode();
        let call = |to: Address, data: Vec<u8>| IRelayAdapt::Call {
            to,
            data: data.into(),
            value: U256::ZERO,
        };
        let approve_to = match funded {
            Funded::OtherToken => self.other_token,
            _ => token_to,
        };
        let mut calls = vec![
            call(approve_to, approve),
            call(open_to, opening.abi_encode()),
            call(self.token, pay),
            call(self.adapt, shield),
        ];
        match funded {
            Funded::ExtraCall => calls.push(call(
                self.token,
                IErc20::approveCall {
                    spender: self.maker,
                    amount: U256::ZERO,
                }
                .abi_encode(),
            )),
            Funded::MissingCall => {
                calls.remove(2);
            }
            Funded::CallValue => calls[2].value = U256::from(1),
            Funded::Reordered => calls.swap(0, 2),
            Funded::InnerTrailing | Funded::InnerSelector => {
                let call = &mut calls[self.seed.below("guard-inner", number as u64, 4) as usize];
                let mut data = call.data.to_vec();
                match funded {
                    Funded::InnerTrailing => data.extend_from_slice(&[0; 32]),
                    _ => data[0] ^= 1,
                }
                call.data = data.into();
            }
            _ => {}
        }
        let action = IRelayAdapt::ActionData {
            random: FixedBytes::<31>::from(self.seed.bytes::<31>(&format!("{tag}-random"))),
            requireSuccess: funded != Funded::NoSuccess,
            minGasLimit: U256::from(match funded {
                Funded::MinGas => super::relayer::MAX_GAS + 1,
                _ => 1_000_000,
            }),
            calls,
        };
        let need = (AMOUNT + FUND_FEE) * 10_000 / 9_975 + 2;
        let unshielded = match funded {
            Funded::Short => (AMOUNT + FUND_FEE) / 2,
            _ => need,
        };
        let coin = self.coins[number][0];
        let exit_token = match funded {
            Funded::UnshieldToken => self.other_token,
            _ => self.token,
        };
        let spends = match funded {
            Funded::NoNullifiers => Vec::new(),
            Funded::UnshieldToken => vec![coin, self.odd[number]],
            _ => vec![coin],
        };
        let mut outs = vec![self.out(self.own, self.token, coin.value - unshielded)];
        if exit_token != self.token {
            outs[0].value = coin.value;
            outs.push(self.out(self.own, exit_token, COIN - unshielded));
        }
        let exit = Exit {
            to: match funded {
                Funded::UnshieldNpk => word(self.maker),
                _ => word(self.adapt),
            },
            token: exit_token,
            value: unshielded,
            kind: 1,
        };
        let exit = match funded {
            Funded::NoUnshield => {
                outs[0].value = coin.value;
                None
            }
            _ => Some(exit),
        };
        let mut draft = self.draft(spends, outs, exit);
        draft.min_gas_price = match funded {
            Funded::MinGasPrice => self.cap + 1,
            _ => 0,
        };
        draft.chain_id = match funded {
            Funded::BoundChain => CHAIN_ID + 1,
            _ => CHAIN_ID,
        };
        draft.adapt = match funded {
            Funded::BoundAdapter => self.other_adapt,
            _ => self.adapt,
        };
        let drafts = match funded {
            Funded::NoTransactions => Vec::new(),
            Funded::Redirect => {
                let mut redirected = draft.clone();
                redirected.spends = vec![self.coins[number][1]];
                redirected.exit = redirected.exit.map(|exit| Exit { kind: 2, ..exit });
                vec![draft, redirected]
            }
            _ => vec![draft],
        };
        let mut transactions: Vec<Transaction> = drafts
            .iter()
            .enumerate()
            .map(|(at, draft)| self.circuit.build(self.seed, &format!("{tag}-{at}"), draft))
            .collect();
        let params = match funded {
            Funded::BoundParams => keccak256(b"another action"),
            _ => adapt_params(&transactions, &action),
        };
        for (transaction, draft) in transactions.iter_mut().zip(&drafts) {
            transaction.boundParams.adaptParams = params;
            self.circuit.prove(transaction, draft);
        }
        let mut data = IRelayAdapt::relayCall {
            _transactions: transactions,
            _actionData: action,
        }
        .abi_encode();
        match funded {
            Funded::Selector => data[0] ^= 1,
            Funded::DirtyBool => {
                let at = 4 + U256::from_be_slice(&data[36..68]).to::<usize>() + 32;
                data[at + 31] = 2;
            }
            _ => {}
        }
        let swap = match funded {
            Funded::SwapId => keccak256(b"another swap"),
            _ => reverse_swap_id(user, &maker_share),
        };
        let post = Post {
            chain_id: CHAIN_ID,
            to: match funded {
                Funded::OtherAdapter => self.other_adapt,
                _ => self.adapt,
            },
            value: u128::from(funded == Funded::WithValue),
            data: Bytes::from(data),
            swap: Some(swap),
        };
        let must = match funded {
            Funded::Honest | Funded::Again => Must::Serve,
            Funded::Short => Must::Fail,
            _ => Must::Refuse,
        };
        (post, must)
    }
}

fn offset(data: &[u8]) -> Vec<u8> {
    let mut out = data[..4].to_vec();
    out.extend_from_slice(&U256::from(0x40).to_be_bytes::<32>());
    out.extend_from_slice(&[0; 32]);
    out.extend_from_slice(&data[36..]);
    out
}
