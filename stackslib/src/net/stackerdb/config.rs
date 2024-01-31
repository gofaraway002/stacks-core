// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2023 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

/// This file implements the interface to the StackerDB smart contract for loading the DB's config.
/// The smart contract must conform to this trait:
///
/// ```clarity,ignore
/// ;; Any StackerDB smart contract must conform to this trait.
/// (define-trait stackerdb-trait
///
///     ;; Get the list of (signer, num-slots) that make up this DB
///     (define-public (stackerdb-get-signer-slots (uint)) (response (list 4096 { signer: principal, num-slots: uint }) uint))
///
///     (define-public (stackerdb-get-page-count) (response uint bool))
///
///     ;; Get the control metadata for this DB
///     (define-public (stackerdb-get-config)
///         (response {
///             chunk-size: uint,
///             write-freq: uint,
///             max-writes: uint,
///             max-neighbors: uint,
///             hint-replicas: (list 128 { addr: (list 16 uint), port: uint, public-key-hash: (buff 20) })
///         },
///         uint))
/// )
/// ```
use std::collections::{HashMap, HashSet};
use std::mem;

use clarity::vm::analysis::ContractAnalysis;
use clarity::vm::clarity::ClarityConnection;
use clarity::vm::database::BurnStateDB;
use clarity::vm::types::{
    BufferLength, FixedFunction, FunctionType, ListTypeData, PrincipalData,
    QualifiedContractIdentifier, SequenceData, SequenceSubtype, StandardPrincipalData,
    TupleTypeSignature, TypeSignature, Value as ClarityValue,
};
use clarity::vm::ClarityName;
use lazy_static::lazy_static;
use stacks_common::types::chainstate::{StacksAddress, StacksBlockId};
use stacks_common::types::net::PeerAddress;
use stacks_common::types::StacksEpochId;
use stacks_common::util::hash::Hash160;

use super::{STACKERDB_PAGE_COUNT_FUNCTION, STACKERDB_PAGE_MAX, STACKERDB_SLOTS_FUNCTION};
use crate::chainstate::burn::db::sortdb::SortitionDB;
use crate::chainstate::nakamoto::NakamotoChainState;
use crate::chainstate::stacks::db::StacksChainState;
use crate::chainstate::stacks::Error as chainstate_error;
use crate::clarity_vm::clarity::{ClarityReadOnlyConnection, Error as clarity_error};
use crate::net::stackerdb::{
    StackerDBConfig, StackerDBs, STACKERDB_INV_MAX, STACKERDB_MAX_CHUNK_SIZE,
};
use crate::net::{Error as NetError, NeighborAddress};

const MAX_HINT_REPLICAS: u32 = 128;

lazy_static! {
    pub static ref REQUIRED_FUNCTIONS: [(ClarityName, Vec<TypeSignature>, TypeSignature); 3] = [
        (
            super::STACKERDB_PAGE_COUNT_FUNCTION.into(),
            vec![],
            TypeSignature::new_response(
                TypeSignature::UIntType,
                TypeSignature::UIntType,
            ).expect("FATAL: failed to construct (response int int)")
        ),
        (
            super::STACKERDB_SLOTS_FUNCTION.into(),
            vec![
                TypeSignature::UIntType
            ],
            TypeSignature::new_response(
                ListTypeData::new_list(
                    TupleTypeSignature::try_from(vec![
                        ("signer".into(), TypeSignature::PrincipalType),
                        ("num-slots".into(), TypeSignature::UIntType)
                    ])
                    .expect("FATAL: failed to construct signer list type")
                    .into(),
                    super::STACKERDB_PAGE_MAX
                )
                .expect("FATAL: could not construct signer list type")
                .into(),
                TypeSignature::UIntType
            ).expect("FATAL: failed to construct response with signer slots"),
        ),
        (
            super::STACKERDB_CONFIG_FUNCTION.into(),
            vec![],
            TypeSignature::new_response(
                TypeSignature::TupleType(
                    TupleTypeSignature::try_from(vec![
                        ("chunk-size".into(), TypeSignature::UIntType),
                        ("write-freq".into(), TypeSignature::UIntType),
                        ("max-writes".into(), TypeSignature::UIntType),
                        ("max-neighbors".into(), TypeSignature::UIntType),
                        ("hint-replicas".into(), ListTypeData::new_list(
                            TypeSignature::TupleType(
                                TupleTypeSignature::try_from(vec![
                                    ("addr".into(), ListTypeData::new_list(TypeSignature::UIntType, 16)
                                        .expect("FATAL: invalid IP address list")
                                        .into()),
                                    ("port".into(), TypeSignature::UIntType),
                                    ("public-key-hash".into(),
                                        // can't use BUFF_20 here because it's also in a
                                        // lazy_static! block
                                        TypeSignature::SequenceType(SequenceSubtype::BufferType(BufferLength::try_from(20u32).expect("FATAL: could not create (buff 20)"))))
                                ])
                                .expect("FATAL: unable to construct hint-replicas type")
                                .into()),
                            MAX_HINT_REPLICAS)
                            .expect("FATAL: failed to construct hint-replicas list type")
                            .into())
                    ]).expect("FATAL: unable to construct config type")).into(),
                TypeSignature::UIntType
            ).expect("FATAL: unable to construct config response type")
        )
    ];
}

impl StackerDBConfig {
    /// Check that a smart contract is consistent with being a StackerDB controller.
    /// Returns Ok(..) if the contract is valid
    /// Returns Err(reason) if the contract is invalid.  A human-readable reason will be given.
    fn is_contract_valid(epoch: &StacksEpochId, analysis: ContractAnalysis) -> Result<(), String> {
        for (name, expected_args, expected_return) in REQUIRED_FUNCTIONS.iter() {
            let func = if let Some(f) = analysis.read_only_function_types.get(name) {
                f
            } else if let Some(f) = analysis.public_function_types.get(name) {
                f
            } else {
                let reason = format!("Contract is missing function '{name}'");
                return Err(reason);
            };

            let FunctionType::Fixed(func) = func else {
                return Err(format!("Function '{name}' must be a fixed function"));
            };

            if func.args.len() != expected_args.len() {
                let reason = format!(
                    "Function '{name}' has an invalid signature: it must have {} args",
                    expected_args.len()
                );
                return Err(reason);
            }
            for (actual_arg, expected_arg) in func.args.iter().zip(expected_args.iter()) {
                if !actual_arg
                    .signature
                    .admits_type(epoch, expected_arg)
                    .unwrap_or(false)
                {
                    return Err(format!("Function '{name}' has an invalid argument type: expected {expected_arg}, got {actual_arg}"));
                }
            }

            if !expected_return
                .admits_type(epoch, &func.returns)
                .unwrap_or(false)
            {
                return Err(format!("Function '{name}' has an invalid return type: expected {expected_return}, got {}", &func.returns));
            }
        }
        Ok(())
    }

    fn eval_page_count(
        chainstate: &mut StacksChainState,
        burn_dbconn: &dyn BurnStateDB,
        contract_id: &QualifiedContractIdentifier,
        tip: &StacksBlockId,
    ) -> Result<u32, NetError> {
        let pages_val = chainstate.eval_fn_read_only(
            burn_dbconn,
            tip,
            contract_id,
            STACKERDB_PAGE_COUNT_FUNCTION,
            &[],
        )?;

        if !matches!(pages_val, ClarityValue::Response(_)) {
            let reason = format!("StackerDB fn `{contract_id}.{STACKERDB_PAGE_COUNT_FUNCTION}` returned unexpected non-response type");
            warn!("{reason}");
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        }

        let ClarityValue::UInt(pages) = pages_val
            .expect_result()
            .map_err(|err_val| {
                let reason = format!(
                    "StackerDB fn `{contract_id}.{STACKERDB_PAGE_COUNT_FUNCTION}` failed: error {err_val}",
                );
                warn!("{reason}");
                NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                )
            })?
        else {
            let reason = format!("StackerDB fn `{contract_id}.{STACKERDB_PAGE_COUNT_FUNCTION}` returned unexpected non-uint ok type");
            warn!("{reason}");
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(), reason));
        };

        pages.try_into().map_err(
            |_| {
                let reason = format!(
                    "StackerDB fn `{contract_id}.{STACKERDB_PAGE_COUNT_FUNCTION}` returned page count outside of u32 range",
                );
                warn!("{reason}");
                NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                )
            }
        )
    }

    fn parse_slot_entry(
        entry: ClarityValue,
        contract_id: &QualifiedContractIdentifier,
    ) -> Result<(StacksAddress, u32), String> {
        let ClarityValue::Tuple(slot_data) = entry else {
            let reason = format!(
                "StackerDB fn `{contract_id}.{STACKERDB_SLOTS_FUNCTION}` returned non-tuple slot entry",
            );
            return Err(reason);
        };

        let Ok(ClarityValue::Principal(signer_principal)) = slot_data.get("signer") else {
            let reason = format!(
                "StackerDB fn `{contract_id}.{STACKERDB_SLOTS_FUNCTION}` returned tuple without `signer` entry of type `principal`",
            );
            return Err(reason);
        };

        let Ok(ClarityValue::UInt(num_slots)) = slot_data.get("num-slots") else {
            let reason = format!(
                "StackerDB fn `{contract_id}.{STACKERDB_SLOTS_FUNCTION}` returned tuple without `num-slots` entry of type `uint`",
            );
            return Err(reason);
        };

        let num_slots = u32::try_from(*num_slots)
            .map_err(|_| format!("Contract `{contract_id}` set too many slots for one signer (max = {STACKERDB_PAGE_MAX})"))?;
        if num_slots > STACKERDB_PAGE_MAX {
            return Err(format!("Contract `{contract_id}` set too many slots for one signer (max = {STACKERDB_PAGE_MAX})"));
        }

        let PrincipalData::Standard(standard_principal) = signer_principal else {
            return Err(format!(
                "StackerDB contract `{contract_id}` set a contract principal as a writer, which is not supported"
            ));
        };
        let addr = StacksAddress::from(standard_principal.clone());
        Ok((addr, num_slots))
    }

    fn eval_signer_slots(
        chainstate: &mut StacksChainState,
        burn_dbconn: &dyn BurnStateDB,
        contract_id: &QualifiedContractIdentifier,
        tip: &StacksBlockId,
    ) -> Result<Vec<(StacksAddress, u32)>, NetError> {
        let page_count = Self::eval_page_count(chainstate, burn_dbconn, contract_id, tip)?;
        if page_count == 0 {
            debug!("StackerDB contract {contract_id} specified zero pages");
            return Ok(vec![]);
        }
        let mut return_set: Option<Vec<(StacksAddress, u32)>> = None;
        let mut total_num_slots = 0u32;
        for page in 0..page_count {
            let (mut new_entries, total_new_slots) =
                Self::eval_signer_slots_page(chainstate, burn_dbconn, contract_id, tip, page)?;
            total_num_slots = total_num_slots
                .checked_add(total_new_slots)
                .ok_or_else(|| {
                    NetError::OverflowError(format!(
                        "Contract {contract_id} set more than u32::MAX slots",
                    ))
                })?;
            if total_num_slots > STACKERDB_INV_MAX {
                let reason =
                    format!("Contract {contract_id} set more than the maximum number of slots in a page (max = {STACKERDB_PAGE_MAX})",);
                warn!("{reason}");
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }
            // avoid buffering on the first page
            if let Some(ref mut return_set) = return_set {
                return_set.append(&mut new_entries);
            } else {
                return_set = Some(new_entries);
            };
        }
        Ok(return_set.unwrap_or_else(|| vec![]))
    }

    /// Evaluate the contract to get its signer slots
    fn eval_signer_slots_page(
        chainstate: &mut StacksChainState,
        burn_dbconn: &dyn BurnStateDB,
        contract_id: &QualifiedContractIdentifier,
        tip: &StacksBlockId,
        page: u32,
    ) -> Result<(Vec<(StacksAddress, u32)>, u32), NetError> {
        let resp_value = chainstate.eval_fn_read_only(
            burn_dbconn,
            tip,
            contract_id,
            STACKERDB_SLOTS_FUNCTION,
            &[ClarityValue::UInt(page.into())],
        )?;

        if !matches!(resp_value, ClarityValue::Response(_)) {
            let reason = format!("StackerDB fn `{contract_id}.{STACKERDB_SLOTS_FUNCTION}` returned unexpected non-response type");
            warn!("{reason}");
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        }

        let slot_list_val = resp_value.expect_result().map_err(|err_val| {
            let reason = format!(
                "StackerDB fn `{contract_id}.{STACKERDB_SLOTS_FUNCTION}` failed: error {err_val}",
            );
            warn!("{reason}");
            NetError::InvalidStackerDBContract(contract_id.clone(), reason)
        })?;

        let slot_list = if let ClarityValue::Sequence(SequenceData::List(list_data)) = slot_list_val
        {
            list_data.data
        } else {
            let reason = format!("StackerDB fn `{contract_id}.{STACKERDB_SLOTS_FUNCTION}` returned unexpected non-list ok type");
            warn!("{reason}");
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        };

        let mut total_num_slots = 0u32;
        let mut ret = vec![];
        for slot_value in slot_list.into_iter() {
            let (addr, num_slots) =
                Self::parse_slot_entry(slot_value, contract_id).map_err(|reason| {
                    warn!("{reason}");
                    NetError::InvalidStackerDBContract(contract_id.clone(), reason)
                })?;

            total_num_slots =
                total_num_slots
                    .checked_add(num_slots)
                    .ok_or(NetError::OverflowError(format!(
                        "Contract {} set more than u32::MAX slots",
                        &contract_id
                    )))?;

            if total_num_slots > STACKERDB_PAGE_MAX.into() {
                let reason = format!(
                    "Contract {} set more than the maximum number of slots",
                    contract_id
                );
                warn!("{}", &reason);
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }

            ret.push((addr, num_slots));
        }
        Ok((ret, total_num_slots))
    }

    /// Evaluate the contract to get its config
    fn eval_config(
        chainstate: &mut StacksChainState,
        burn_dbconn: &dyn BurnStateDB,
        contract_id: &QualifiedContractIdentifier,
        tip: &StacksBlockId,
        signers: Vec<(StacksAddress, u32)>,
    ) -> Result<StackerDBConfig, NetError> {
        let value =
            chainstate.eval_read_only(burn_dbconn, tip, contract_id, "(stackerdb-get-config)")?;

        let result = value.expect_result();
        let config_tuple = match result {
            Err(err_val) => {
                let err_code = err_val.expect_u128();
                let reason = format!(
                    "Contract {} failed to run `stackerdb-get-config`: err u{}",
                    contract_id, &err_code
                );
                warn!("{}", &reason);
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }
            Ok(ok_val) => ok_val.expect_tuple(),
        };

        let chunk_size = config_tuple
            .get("chunk-size")
            .expect("FATAL: missing 'chunk-size'")
            .clone()
            .expect_u128();

        if chunk_size > STACKERDB_MAX_CHUNK_SIZE as u128 {
            let reason = format!(
                "Contract {} stipulates a chunk size beyond STACKERDB_MAX_CHUNK_SIZE",
                contract_id
            );
            warn!("{}", &reason);
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        }

        let write_freq = config_tuple
            .get("write-freq")
            .expect("FATAL: missing 'write-freq'")
            .clone()
            .expect_u128();
        if write_freq > u64::MAX as u128 {
            let reason = format!(
                "Contract {} stipulates a write frequency beyond u64::MAX",
                contract_id
            );
            warn!("{}", &reason);
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        }

        let max_writes = config_tuple
            .get("max-writes")
            .expect("FATAL: missing 'max-writes'")
            .clone()
            .expect_u128();
        if max_writes > u32::MAX as u128 {
            let reason = format!(
                "Contract {} stipulates a max-write bound beyond u32::MAX",
                contract_id
            );
            warn!("{}", &reason);
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        }

        let max_neighbors = config_tuple
            .get("max-neighbors")
            .expect("FATAL: missing 'max-neighbors'")
            .clone()
            .expect_u128();
        if max_neighbors > usize::MAX as u128 {
            let reason = format!(
                "Contract {} stipulates a maximum number of neighbors beyond usize::MAX",
                contract_id
            );
            warn!("{}", &reason);
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        }

        let hint_replicas_list = config_tuple
            .get("hint-replicas")
            .expect("FATAL: missing 'hint-replicas'")
            .clone()
            .expect_list();
        let mut hint_replicas = vec![];
        for hint_replica_value in hint_replicas_list.into_iter() {
            let hint_replica_data = hint_replica_value.expect_tuple();

            let addr_byte_list = hint_replica_data
                .get("addr")
                .expect("FATAL: missing 'addr'")
                .clone()
                .expect_list();
            let port = hint_replica_data
                .get("port")
                .expect("FATAL: missing 'port'")
                .clone()
                .expect_u128();
            let pubkey_hash_bytes = hint_replica_data
                .get("public-key-hash")
                .expect("FATAL: missing 'public-key-hash")
                .clone()
                .expect_buff_padded(20, 0);

            let mut addr_bytes = vec![];
            for byte_val in addr_byte_list.into_iter() {
                let byte = byte_val.expect_u128();
                if byte > (u8::MAX as u128) {
                    let reason = format!(
                        "Contract {} stipulates an addr byte above u8::MAX",
                        contract_id
                    );
                    warn!("{}", &reason);
                    return Err(NetError::InvalidStackerDBContract(
                        contract_id.clone(),
                        reason,
                    ));
                }
                addr_bytes.push(byte as u8);
            }
            if addr_bytes.len() != 16 {
                let reason = format!(
                    "Contract {} did not stipulate a full 16-octet IP address",
                    contract_id
                );
                warn!("{}", &reason);
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }

            if port < 1024 || port > ((u16::MAX - 1) as u128) {
                let reason = format!(
                    "Contract {} stipulates a port lower than 1024 or above u16::MAX - 1",
                    contract_id
                );
                warn!("{}", &reason);
                return Err(NetError::InvalidStackerDBContract(
                    contract_id.clone(),
                    reason,
                ));
            }

            let mut pubkey_hash_slice = [0u8; 20];
            pubkey_hash_slice.copy_from_slice(&pubkey_hash_bytes[0..20]);

            let peer_addr = PeerAddress::from_slice(&addr_bytes).expect("FATAL: not 16 bytes");
            let naddr = NeighborAddress {
                addrbytes: peer_addr,
                port: port as u16,
                public_key_hash: Hash160(pubkey_hash_slice),
            };
            hint_replicas.push(naddr);
        }

        Ok(StackerDBConfig {
            chunk_size: chunk_size as u64,
            signers,
            write_freq: write_freq as u64,
            max_writes: max_writes as u32,
            hint_replicas,
            max_neighbors: max_neighbors as usize,
        })
    }

    /// Load up the DB config from the controlling smart contract as of the current Stacks chain
    /// tip
    pub fn from_smart_contract(
        chainstate: &mut StacksChainState,
        sortition_db: &SortitionDB,
        contract_id: &QualifiedContractIdentifier,
    ) -> Result<StackerDBConfig, NetError> {
        let chain_tip =
            NakamotoChainState::get_canonical_block_header(chainstate.db(), sortition_db)?
                .ok_or(NetError::NoSuchStackerDB(contract_id.clone()))?;

        let burn_tip = SortitionDB::get_block_snapshot_consensus(
            sortition_db.conn(),
            &chain_tip.consensus_hash,
        )?
        .expect("FATAL: missing snapshot for Stacks block");

        let chain_tip_hash = StacksBlockId::new(
            &chain_tip.consensus_hash,
            &chain_tip.anchored_header.block_hash(),
        );
        let cur_epoch = SortitionDB::get_stacks_epoch(sortition_db.conn(), burn_tip.block_height)?
            .expect("FATAL: no epoch defined");

        let dbconn = sortition_db.index_conn();

        // check the target contract
        let res =
            chainstate.maybe_read_only_clarity_tx(&dbconn, &chain_tip_hash, |clarity_tx| {
                // determine if this contract exists and conforms to this trait
                clarity_tx.with_clarity_db_readonly(|db| {
                    // contract must exist or this errors out
                    let analysis = db
                        .load_contract_analysis(contract_id)
                        .ok_or(NetError::NoSuchStackerDB(contract_id.clone()))?;

                    // contract must be consistent with StackerDB control interface
                    if let Err(invalid_reason) =
                        Self::is_contract_valid(&cur_epoch.epoch_id, analysis)
                    {
                        let reason = format!(
                            "Contract {} does not conform to StackerDB trait: {}",
                            contract_id, invalid_reason
                        );
                        warn!("{}", &reason);
                        return Err(NetError::InvalidStackerDBContract(
                            contract_id.clone(),
                            reason,
                        ));
                    }

                    Ok(())
                })
            })?;

        if res.is_none() {
            let reason = format!(
                "Could not evaluate contract {} at {}",
                contract_id, &chain_tip_hash
            );
            warn!("{}", &reason);
            return Err(NetError::InvalidStackerDBContract(
                contract_id.clone(),
                reason,
            ));
        } else if let Some(Err(e)) = res {
            warn!(
                "Could not use contract {} for StackerDB: {:?}",
                contract_id, &e
            );
            return Err(e);
        }

        // evaluate the contract for these two functions
        let signers = Self::eval_signer_slots(chainstate, &dbconn, contract_id, &chain_tip_hash)?;
        let config = Self::eval_config(chainstate, &dbconn, contract_id, &chain_tip_hash, signers)?;
        Ok(config)
    }
}
