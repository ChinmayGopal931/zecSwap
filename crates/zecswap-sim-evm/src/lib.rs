use revm::context::result::ExecutionResult;
use revm::context::{BlockEnv, TxEnv};
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::hardfork::SpecId;
use revm::state::EvmState;
use revm::{Context, ExecuteEvm, MainBuilder, MainContext};

pub type State = CacheDB<EmptyDB>;

pub struct Ran {
    pub result: ExecutionResult,
    pub state: EvmState,
}

pub fn run(
    state: &State,
    chain_id: u64,
    block: &BlockEnv,
    tx: TxEnv,
    relaxed: bool,
) -> Result<Ran, String> {
    let mut evm = Context::mainnet()
        .with_ref_db(state)
        .with_block(block.clone())
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = chain_id;
            cfg.spec = SpecId::OSAKA;
            cfg.tx_gas_limit_cap = Some(u64::MAX);
            if relaxed {
                cfg.disable_base_fee = true;
                cfg.disable_balance_check = true;
                cfg.disable_nonce_check = true;
            }
        })
        .build_mainnet();
    let ran = evm.transact(tx).map_err(|e| e.to_string())?;
    Ok(Ran {
        result: ran.result,
        state: ran.state,
    })
}
