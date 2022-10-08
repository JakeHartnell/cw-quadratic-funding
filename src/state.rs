use crate::matching::QuadraticFundingAlgorithm;
use cosmwasm_schema::cw_serde;
use cosmwasm_std::{Addr, Binary, Coin, Storage, Uint128};
use cosmwasm_storage::{singleton, Singleton};
use cw_storage_plus::{Item, Map};
use cw_utils::Expiration;

#[cw_serde]
pub struct Config {
    // set admin as single address, multisig or contract sig could be used
    pub admin: Addr,
    // leftover coins from distribution sent to this address
    pub leftover_addr: Addr,
    pub create_proposal_whitelist: Option<Vec<Addr>>,
    pub vote_proposal_whitelist: Option<Vec<Addr>>,
    pub voting_period: Expiration,
    pub proposal_period: Expiration,
    pub budget: Coin,
    pub algorithm: QuadraticFundingAlgorithm,
}

pub const CONFIG: Item<Config> = Item::new("config");

#[cw_serde]
pub struct Proposal {
    pub id: u64,
    pub title: String,
    pub description: String,
    pub metadata: Option<Binary>,
    pub fund_address: Addr,
    pub collected_funds: Uint128,
}

pub const PROPOSALS: Map<u64, Proposal> = Map::new("proposal");
pub const PROPOSAL_SEQ: &[u8] = b"proposal_seq";

pub fn proposal_seq(storage: &mut dyn Storage) -> Singleton<u64> {
    singleton(storage, PROPOSAL_SEQ)
}

#[cw_serde]
pub struct Vote {
    pub proposal_id: u64,
    pub voter: String,
    pub fund: Coin,
}

pub const VOTES: Map<(u64, &[u8]), Vote> = Map::new("votes");
