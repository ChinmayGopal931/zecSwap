use super::*;

const SEPOLIA_CHAIN_ID: u64 = 11_155_111;

fn fixture() -> (FundingPolicy, Domain, B256, IRelayAdapt::relayCall) {
    let json: serde_json::Value = serde_json::from_str(include_str!("sdk-vector.json")).unwrap();
    let address = |key: &str| json[key].as_str().unwrap().parse::<Address>().unwrap();
    let policy = FundingPolicy {
        relay_adapt: address("to"),
        token: address("token"),
        maker: address("maker"),
        max_gas_limit: 4_000_000,
        max_gas_price_wei: 20_000_000_000,
        fee: json["fee"].as_str().unwrap().parse().unwrap(),
        fee_recipient: address("relayer"),
    };
    let domain = Domain {
        chain_id: SEPOLIA_CHAIN_ID,
        contract: address("contract").into(),
    };
    let data: Bytes = json["data"].as_str().unwrap().parse().unwrap();
    let id = json["swapId"].as_str().unwrap().parse().unwrap();
    (
        policy,
        domain,
        id,
        IRelayAdapt::relayCall::abi_decode_validate(&data).unwrap(),
    )
}

fn validate(
    p: &FundingPolicy,
    d: Domain,
    id: B256,
    relay: &IRelayAdapt::relayCall,
) -> Result<ValidatedFunding> {
    p.validate(d, id, p.relay_adapt, 0, relay.abi_encode().into())
}

#[test]
fn accepts_independently_encoded_sdk_vector() {
    let (p, d, id, relay) = fixture();
    let validated = validate(&p, d, id, &relay).unwrap();
    assert_eq!(validated.terms.amount, 1_000_000);
    assert_eq!(validated.min_gas_price, 1);
}
