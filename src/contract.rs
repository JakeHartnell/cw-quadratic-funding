use cosmwasm_std::{
    attr, coin, to_binary, Addr, BankMsg, Binary, CosmosMsg, Deps, DepsMut, Env, MessageInfo,
    Order, Response, StdResult,
};
use cosmwasm_std::{entry_point, Uint128};

use crate::error::ContractError;
use crate::helper::extract_budget_coin;
use crate::matching::{calculate_clr, QuadraticFundingAlgorithm, RawGrant};
use crate::msg::{AllProposalsResponse, ExecuteMsg, InstantiateMsg, QueryMsg};
use crate::state::{proposal_seq, Config, Proposal, Vote, CONFIG, PROPOSALS, VOTES};
use cosmwasm_storage::nextval;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> Result<Response, ContractError> {
    msg.validate(env)?;

    let budget = extract_budget_coin(info.funds.as_slice(), &msg.budget_denom)?;
    let mut create_proposal_whitelist: Option<Vec<Addr>> = None;
    let mut vote_proposal_whitelist: Option<Vec<Addr>> = None;
    if let Some(pwl) = msg.create_proposal_whitelist {
        let mut tmp_wl = vec![];
        for w in pwl {
            tmp_wl.push(deps.api.addr_validate(&w)?)
        }
        create_proposal_whitelist = Some(tmp_wl);
    }
    if let Some(vwl) = msg.vote_proposal_whitelist {
        let mut tmp_wl = vec![];
        for w in vwl {
            tmp_wl.push(deps.api.addr_validate(&w)?)
        }
        vote_proposal_whitelist = Some(tmp_wl);
    }
    let cfg = Config {
        admin: deps.api.addr_validate(&msg.admin)?,
        leftover_addr: deps.api.addr_validate(&msg.leftover_addr)?,
        create_proposal_whitelist,
        vote_proposal_whitelist,
        voting_period: msg.voting_period,
        proposal_period: msg.proposal_period,
        algorithm: msg.algorithm,
        budget,
    };
    CONFIG.save(deps.storage, &cfg)?;

    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: ExecuteMsg,
) -> Result<Response, ContractError> {
    match msg {
        ExecuteMsg::CreateProposal {
            title,
            description,
            metadata,
            fund_address,
        } => execute_create_proposal(deps, env, info, title, description, metadata, fund_address),
        ExecuteMsg::VoteProposal { proposal_id } => {
            execute_vote_proposal(deps, env, info, proposal_id)
        }
        ExecuteMsg::TriggerDistribution { .. } => execute_trigger_distribution(deps, env, info),
    }
}

pub fn execute_create_proposal(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    title: String,
    description: String,
    metadata: Option<Binary>,
    fund_address: String,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    // check whitelist
    if let Some(wl) = config.create_proposal_whitelist {
        if !wl.contains(&info.sender) {
            return Err(ContractError::Unauthorized {});
        }
    }

    // check proposal expiration
    if config.proposal_period.is_expired(&env.block) {
        return Err(ContractError::ProposalPeriodExpired {});
    }

    let id = nextval(&mut proposal_seq(deps.storage))?;
    let p = Proposal {
        id,
        title: title.clone(),
        description,
        metadata,
        fund_address: deps.api.addr_validate(&fund_address)?,
        collected_funds: Uint128::zero(),
    };
    PROPOSALS.save(deps.storage, id.into(), &p)?;

    Ok(Response::new()
        .add_attribute("action", "create_proposal")
        .add_attribute("title", title)
        .add_attribute("proposal_id", id.to_string()))
}

pub fn execute_vote_proposal(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    proposal_id: u64,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    // check whitelist
    if let Some(wl) = config.vote_proposal_whitelist {
        if !wl.contains(&info.sender) {
            return Err(ContractError::Unauthorized {});
        }
    }

    // check voting expiration
    if config.voting_period.is_expired(&env.block) {
        return Err(ContractError::VotingPeriodExpired {});
    }

    // validate sent funds and funding denom matches
    let fund = extract_budget_coin(&info.funds, &config.budget.denom)?;

    // check existence of the proposal and collect funds in proposal
    let proposal = PROPOSALS.update(deps.storage, proposal_id.into(), |op| match op {
        None => Err(ContractError::ProposalNotFound {}),
        Some(mut proposal) => {
            proposal.collected_funds += fund.amount;
            Ok(proposal)
        }
    })?;

    let vote = Vote {
        proposal_id,
        voter: info.sender.to_string(),
        fund,
    };

    // check sender did not voted on proposal
    let vote_key = VOTES.key((proposal_id.into(), info.sender.as_bytes()));
    if vote_key.may_load(deps.storage)?.is_some() {
        return Err(ContractError::AddressAlreadyVotedProject {});
    }

    // save vote
    vote_key.save(deps.storage, &vote)?;

    Ok(Response::default().add_attributes(vec![
        attr("action", "vote_proposal"),
        attr("proposal_key", proposal_id.to_string()),
        attr("voter", vote.voter),
        attr("collected_fund", proposal.collected_funds),
    ]))
}

pub fn execute_trigger_distribution(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
) -> Result<Response, ContractError> {
    let config = CONFIG.load(deps.storage)?;

    // only admin can trigger distribution
    if info.sender != config.admin {
        return Err(ContractError::Unauthorized {});
    }

    // check voting period expiration
    if !config.voting_period.is_expired(&env.block) {
        return Err(ContractError::VotingPeriodNotExpired {});
    }

    let query_proposals: StdResult<Vec<_>> = PROPOSALS
        .range(deps.storage, None, None, Order::Ascending)
        .collect();

    let proposals: Vec<Proposal> = query_proposals?.into_iter().map(|p| p.1).collect();

    let mut grants: Vec<RawGrant> = vec![];
    // collect proposals under grants
    for p in proposals {
        let vote_query: StdResult<Vec<(Vec<u8>, Vote)>> = VOTES
            .prefix(p.id.into())
            .range(deps.storage, None, None, Order::Ascending)
            .collect();

        let mut votes: Vec<u128> = vec![];
        for v in vote_query? {
            votes.push(v.1.fund.amount.u128());
        }
        let grant = RawGrant {
            addr: p.fund_address,
            funds: votes,
            collected_vote_funds: p.collected_funds.u128(),
        };

        grants.push(grant);
    }

    let (distr_funds, leftover) = match config.algorithm {
        QuadraticFundingAlgorithm::CapitalConstrainedLiberalRadicalism { .. } => {
            calculate_clr(grants, Some(config.budget.amount.u128()))?
        }
    };

    let mut msgs = vec![];
    for f in distr_funds {
        msgs.push(CosmosMsg::Bank(BankMsg::Send {
            to_address: f.addr.to_string(),
            amount: vec![coin(f.grant + f.collected_vote_funds, &config.budget.denom)],
        }));
    }

    let leftover_msg: CosmosMsg = CosmosMsg::Bank(BankMsg::Send {
        to_address: config.leftover_addr.to_string(),
        amount: vec![coin(leftover, config.budget.denom)],
    });

    msgs.push(leftover_msg);

    Ok(Response::new()
        .add_messages(msgs)
        .add_attribute("action", "trigger_distribution"))
}

pub fn query(deps: Deps, _env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::ProposalByID { id } => to_binary(&query_proposal_id(deps, id)?),
        QueryMsg::AllProposals {} => to_binary(&query_all_proposals(deps)?),
    }
}

fn query_proposal_id(deps: Deps, id: u64) -> StdResult<Proposal> {
    PROPOSALS.load(deps.storage, id.into())
}

fn query_all_proposals(deps: Deps) -> StdResult<AllProposalsResponse> {
    let all: StdResult<Vec<(u64, Proposal)>> = PROPOSALS
        .range(deps.storage, None, None, Order::Ascending)
        .collect();
    all.map(|p| {
        let res = p.into_iter().map(|x| x.1).collect();

        AllProposalsResponse { proposals: res }
    })
}

#[cfg(test)]
mod tests {
    use crate::contract::{execute, instantiate, query_all_proposals, query_proposal_id};
    use crate::error::ContractError;
    use crate::matching::QuadraticFundingAlgorithm;
    use crate::msg::{AllProposalsResponse, ExecuteMsg, InstantiateMsg};
    use crate::state::{Proposal, PROPOSALS};
    use cosmwasm_std::testing::{mock_dependencies, mock_env, mock_info};
    use cosmwasm_std::{coin, Addr, BankMsg, Binary, CosmosMsg, Uint128};
    use cw_utils::Expiration;

    #[test]
    fn create_proposal() {
        let mut env = mock_env();
        let info = mock_info("addr", &[coin(1000, "ucosm")]);
        let mut deps = mock_dependencies();

        let init_msg = InstantiateMsg {
            admin: String::from("addr"),
            leftover_addr: String::from("addr"),
            create_proposal_whitelist: None,
            vote_proposal_whitelist: None,
            voting_period: Expiration::AtHeight(env.block.height + 15),
            proposal_period: Expiration::AtHeight(env.block.height + 10),
            budget_denom: String::from("ucosm"),
            algorithm: QuadraticFundingAlgorithm::CapitalConstrainedLiberalRadicalism {
                parameter: "".to_string(),
            },
        };

        instantiate(deps.as_mut(), env.clone(), info.clone(), init_msg.clone()).unwrap();
        let msg = ExecuteMsg::CreateProposal {
            title: String::from("test"),
            description: String::from("test"),
            metadata: Some(b"test".into()),
            fund_address: String::from("fund_address"),
        };

        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        assert!(res.is_ok());

        // proposal period expired
        env.block.height = env.block.height + 1000;
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());

        match res {
            Ok(_) => panic!("expected error"),
            Err(ContractError::ProposalPeriodExpired {}) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }

        // unauthorised
        let env = mock_env();
        let info = mock_info("true", &[coin(1000, "ucosm")]);
        let mut deps = mock_dependencies();
        let init_msg = InstantiateMsg {
            leftover_addr: String::from("addr"),
            admin: String::from("person"),
            create_proposal_whitelist: Some(vec![String::from("false")]),
            vote_proposal_whitelist: None,
            voting_period: Default::default(),
            proposal_period: Default::default(),
            budget_denom: String::from("ucosm"),
            algorithm: QuadraticFundingAlgorithm::CapitalConstrainedLiberalRadicalism {
                parameter: "".to_string(),
            },
        };
        instantiate(deps.as_mut(), env.clone(), info.clone(), init_msg.clone()).unwrap();

        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());

        match res {
            Ok(_) => panic!("expected error"),
            Err(ContractError::Unauthorized {}) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }
    }

    #[test]
    fn vote_proposal() {
        let mut env = mock_env();
        let info = mock_info("addr", &[coin(1000, "ucosm")]);
        let mut deps = mock_dependencies();

        let mut init_msg = InstantiateMsg {
            leftover_addr: String::from("addr"),
            algorithm: QuadraticFundingAlgorithm::CapitalConstrainedLiberalRadicalism {
                parameter: "".to_string(),
            },
            admin: String::from("addr"),
            create_proposal_whitelist: None,
            vote_proposal_whitelist: None,
            voting_period: Expiration::AtHeight(env.block.height + 15),
            proposal_period: Expiration::AtHeight(env.block.height + 10),
            budget_denom: String::from("ucosm"),
        };
        instantiate(deps.as_mut(), env.clone(), info.clone(), init_msg.clone()).unwrap();

        let create_proposal_msg = ExecuteMsg::CreateProposal {
            title: String::from("test"),
            description: String::from("test"),
            metadata: Some(Binary::from(b"test")),
            fund_address: String::from("fund_address"),
        };

        let res = execute(
            deps.as_mut(),
            env.clone(),
            info.clone(),
            create_proposal_msg.clone(),
        );
        assert!(res.is_ok());

        let msg = ExecuteMsg::VoteProposal { proposal_id: 1 };
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        // success case
        match res {
            Ok(_) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }

        // double vote prevention
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        match res {
            Ok(_) => panic!("expected error"),
            Err(ContractError::AddressAlreadyVotedProject {}) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }

        // whitelist check
        let mut deps = mock_dependencies();
        init_msg.vote_proposal_whitelist = Some(vec![String::from("admin")]);
        instantiate(deps.as_mut(), env.clone(), info.clone(), init_msg.clone()).unwrap();
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        match res {
            Ok(_) => panic!("expected error"),
            Err(ContractError::Unauthorized {}) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }

        // proposal period expired
        let mut deps = mock_dependencies();
        init_msg.vote_proposal_whitelist = None;
        instantiate(deps.as_mut(), env.clone(), info.clone(), init_msg.clone()).unwrap();
        env.block.height = env.block.height + 15;
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());

        match res {
            Ok(_) => panic!("expected error"),
            Err(ContractError::VotingPeriodExpired {}) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }
    }

    #[test]
    fn trigger_distribution() {
        let env = mock_env();
        let budget = 550000u128;
        let info = mock_info("admin", &[coin(budget, "ucosm")]);
        let mut deps = mock_dependencies();

        let init_msg = InstantiateMsg {
            leftover_addr: String::from("addr"),
            algorithm: QuadraticFundingAlgorithm::CapitalConstrainedLiberalRadicalism {
                parameter: "".to_string(),
            },
            admin: String::from("admin"),
            create_proposal_whitelist: None,
            vote_proposal_whitelist: None,
            voting_period: Expiration::AtHeight(env.block.height + 15),
            proposal_period: Expiration::AtHeight(env.block.height + 10),
            budget_denom: String::from("ucosm"),
        };

        instantiate(deps.as_mut(), env.clone(), info.clone(), init_msg.clone()).unwrap();

        // insert proposals
        let msg = ExecuteMsg::CreateProposal {
            title: String::from("proposal 1"),
            description: "".to_string(),
            metadata: Some(Binary::from(b"test")),
            fund_address: String::from("fund_address1"),
        };
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        assert!(res.is_ok());

        let msg = ExecuteMsg::CreateProposal {
            title: String::from("proposal 2"),
            description: "".to_string(),
            metadata: Some(Binary::from(b"test")),
            fund_address: String::from("fund_address2"),
        };
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        assert!(res.is_ok());

        let msg = ExecuteMsg::CreateProposal {
            title: String::from("proposal 3"),
            description: "".to_string(),
            metadata: Some(Binary::from(b"test")),
            fund_address: String::from("fund_address3"),
        };
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        assert!(res.is_ok());

        let msg = ExecuteMsg::CreateProposal {
            title: String::from("proposal 4"),
            description: "".to_string(),
            metadata: Some(Binary::from(b"test")),
            fund_address: String::from("fund_address4"),
        };
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        assert!(res.is_ok());

        // insert votes
        // proposal1
        let msg = ExecuteMsg::VoteProposal { proposal_id: 1 };
        let vote11_fund = 1200u128;
        let info = mock_info("address1", &[coin(vote11_fund, "ucosm")]);
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        match res {
            Ok(_) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }

        let vote12_fund = 44999u128;
        let info = mock_info("address2", &[coin(vote12_fund, "ucosm")]);
        execute(deps.as_mut(), env.clone(), info.clone(), msg.clone()).unwrap();
        let vote13_fund = 33u128;
        let info = mock_info("address3", &[coin(vote13_fund, "ucosm")]);
        execute(deps.as_mut(), env.clone(), info.clone(), msg.clone()).unwrap();
        let proposal1 = vote11_fund + vote12_fund + vote13_fund;

        // proposal2
        let msg = ExecuteMsg::VoteProposal { proposal_id: 2 };

        let vote21_fund = 30000u128;
        let info = mock_info("address4", &[coin(vote21_fund, "ucosm")]);
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        match res {
            Ok(_) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }
        let vote22_fund = 58999u128;
        let info = mock_info("address5", &[coin(vote22_fund, "ucosm")]);
        execute(deps.as_mut(), env.clone(), info.clone(), msg.clone()).unwrap();
        let proposal2 = vote21_fund + vote22_fund;

        // proposal3
        let msg = ExecuteMsg::VoteProposal { proposal_id: 3 };
        let vote31_fund = 230000u128;
        let info = mock_info("address6", &[coin(vote31_fund, "ucosm")]);
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        match res {
            Ok(_) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }
        let vote32_fund = 100u128;
        let info = mock_info("address7", &[coin(vote32_fund, "ucosm")]);
        execute(deps.as_mut(), env.clone(), info.clone(), msg.clone()).unwrap();
        let proposal3 = vote31_fund + vote32_fund;

        // proposal4
        let msg = ExecuteMsg::VoteProposal { proposal_id: 4 };
        let vote41_fund = 100000u128;
        let info = mock_info("address8", &[coin(vote41_fund, "ucosm")]);
        let res = execute(deps.as_mut(), env.clone(), info.clone(), msg.clone());
        match res {
            Ok(_) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }
        let vote42_fund = 5u128;
        let info = mock_info("address9", &[coin(vote42_fund, "ucosm")]);
        execute(deps.as_mut(), env.clone(), info.clone(), msg.clone()).unwrap();
        let proposal4 = vote41_fund + vote42_fund;

        let trigger_msg = ExecuteMsg::TriggerDistribution {};
        let info = mock_info("admin", &[]);
        let mut env = mock_env();
        env.block.height += 1000;
        let res = execute(deps.as_mut(), env.clone(), info, trigger_msg);

        let expected_msgs: Vec<CosmosMsg<_>> = vec![
            CosmosMsg::Bank(BankMsg::Send {
                to_address: String::from("fund_address1"),
                amount: vec![coin(106444u128, "ucosm")],
            }),
            CosmosMsg::Bank(BankMsg::Send {
                to_address: String::from("fund_address2"),
                amount: vec![coin(253601u128, "ucosm")],
            }),
            CosmosMsg::Bank(BankMsg::Send {
                to_address: String::from("fund_address3"),
                amount: vec![coin(458637u128, "ucosm")],
            }),
            CosmosMsg::Bank(BankMsg::Send {
                to_address: String::from("fund_address4"),
                amount: vec![coin(196653u128, "ucosm")],
            }),
            // left over msg
            CosmosMsg::Bank(BankMsg::Send {
                to_address: String::from("addr"),
                amount: vec![coin(1u128, "ucosm")],
            }),
        ];
        match res {
            Ok(_) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }

        // check total cash in and out
        let expected_msg_total_distr: u128 = expected_msgs
            .into_iter()
            .map(|d: CosmosMsg<BankMsg>| -> u128 {
                match d {
                    CosmosMsg::Bank(BankMsg::Send { amount, .. }) => {
                        amount.iter().map(|c| c.amount.u128()).sum()
                    }
                    _ => unimplemented!(),
                }
            })
            .collect::<Vec<u128>>()
            .iter()
            .sum();
        let total_fund = proposal1 + proposal2 + proposal3 + proposal4 + budget;

        assert_eq!(total_fund, expected_msg_total_distr)
    }

    #[test]
    fn query_proposal() {
        let mut deps = mock_dependencies();

        let proposal = Proposal {
            id: 1,
            title: "title".to_string(),
            description: "desc".to_string(),
            metadata: None,
            fund_address: Addr::unchecked("proposal1"),
            collected_funds: Uint128::zero(),
        };

        let err = PROPOSALS.save(&mut deps.storage, 1_u64.into(), &proposal);
        match err {
            Ok(_) => {}
            e => panic!("unexpected error, got {}", e.unwrap_err()),
        }
        let res = query_proposal_id(deps.as_ref(), 1).unwrap();
        assert_eq!(proposal, res);
    }

    #[test]
    fn query_all_proposal() {
        let mut deps = mock_dependencies();

        let proposal = Proposal {
            id: 1,
            title: "title".to_string(),
            description: "desc".to_string(),
            metadata: None,
            fund_address: Addr::unchecked("proposal1"),
            collected_funds: Uint128::zero(),
        };
        let _ = PROPOSALS.save(&mut deps.storage, 1_u64.into(), &proposal);

        let proposal1 = Proposal {
            id: 2,
            title: "title 2".to_string(),
            description: "desc".to_string(),
            metadata: None,
            fund_address: Addr::unchecked("proposal2"),
            collected_funds: Uint128::zero(),
        };
        let _ = PROPOSALS.save(&mut deps.storage, 2_u64.into(), &proposal1);
        let res = query_all_proposals(deps.as_ref()).unwrap();

        assert_eq!(
            AllProposalsResponse {
                proposals: vec![proposal, proposal1]
            },
            res
        );
    }
}
