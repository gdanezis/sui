// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet, HashSet};

use move_binary_format::file_format::AbilitySet;
use move_core_types::{
    identifier::IdentStr,
    resolver::{ModuleResolver, ResourceResolver},
};
use move_vm_types::loaded_data::runtime_types::Type;
use once_cell::sync::Lazy;
use serde::Deserialize;

use crate::{
    base_types::{ObjectID, SequenceNumber, SuiAddress},
    coin::Coin,
    digests::{ObjectDigest, TransactionDigest},
    error::{ExecutionError, ExecutionErrorKind, SuiError},
    event::Event,
    execution_status::CommandArgumentError,
    object::{Object, Owner},
    storage::{BackingPackageStore, ChildObjectResolver, ObjectChange, StorageView},
    transfer::Receiving,
};

pub trait SuiResolver:
    ResourceResolver<Error = SuiError> + ModuleResolver<Error = SuiError> + BackingPackageStore
{
    fn as_backing_package_store(&self) -> &dyn BackingPackageStore;
}

impl<T> SuiResolver for T
where
    T: ResourceResolver<Error = SuiError>,
    T: ModuleResolver<Error = SuiError>,
    T: BackingPackageStore,
{
    fn as_backing_package_store(&self) -> &dyn BackingPackageStore {
        self
    }
}

/// Interface with the store necessary to execute a programmable transaction
pub trait ExecutionState: StorageView + SuiResolver {
    fn as_sui_resolver(&self) -> &dyn SuiResolver;
    fn as_child_resolver(&self) -> &dyn ChildObjectResolver;
}

impl<T> ExecutionState for T
where
    T: StorageView,
    T: SuiResolver,
{
    fn as_sui_resolver(&self) -> &dyn SuiResolver {
        self
    }

    fn as_child_resolver(&self) -> &dyn ChildObjectResolver {
        self
    }
}

/// View of the store necessary to produce the layouts of types.
pub trait TypeLayoutStore: BackingPackageStore + ModuleResolver<Error = SuiError> {}
impl<T> TypeLayoutStore for T
where
    T: BackingPackageStore,
    T: ModuleResolver<Error = SuiError>,
{
}

#[derive(Debug)]
pub enum ExecutionResults {
    V1(ExecutionResultsV1),
    V2(ExecutionResultsV2),
}

#[derive(Debug)]
pub struct ExecutionResultsV1 {
    pub object_changes: BTreeMap<ObjectID, ObjectChange>,
    pub user_events: Vec<Event>,
}

/// Used by sui-execution v1 and above, to capture the execution results from Move.
/// The results represent the primitive information that can then be used to construct
/// both transaction effects V1 and V2.
#[derive(Debug)]
pub struct ExecutionResultsV2 {
    /// All objects written regardless of whether they were mutated, created, or unwrapped.
    pub written_objects: BTreeMap<ObjectID, Object>,
    /// All objects that existed prior to this transaction, and are modified in this transaction.
    /// This includes any type of modification, including mutated, wrapped and deleted objects.
    pub modified_objects: BTreeSet<ObjectID>,
    /// All object IDs created in this transaction.
    pub created_object_ids: BTreeSet<ObjectID>,
    /// All object IDs deleted in this transaction.
    /// No object ID should be in both created_object_ids and deleted_object_ids.
    pub deleted_object_ids: BTreeSet<ObjectID>,
    /// All Move events emitted in this transaction.
    pub user_events: Vec<Event>,
}

#[derive(Clone, Debug)]
pub enum InputObjectMetadata {
    Receiving {
        id: ObjectID,
        version: SequenceNumber,
    },
    InputObject {
        id: ObjectID,
        is_mutable_input: bool,
        owner: Owner,
        version: SequenceNumber,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DynamicallyLoadedObjectMetadata {
    pub version: SequenceNumber,
    pub digest: ObjectDigest,
    pub owner: Owner,
    pub storage_rebate: u64,
    pub previous_transaction: TransactionDigest,
}

#[derive(Clone, Debug)]
pub struct InputValue {
    /// Used to remember the object ID and owner even if the value is taken
    pub object_metadata: Option<InputObjectMetadata>,
    pub inner: ResultValue,
}

#[derive(Clone, Debug)]
pub struct ResultValue {
    /// This is used primarily for values that have `copy` but not `drop` as they must have been
    /// copied after the last borrow, otherwise we cannot consider the last "copy" to be instead
    /// a "move" of the value.
    pub last_usage_kind: Option<UsageKind>,
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageKind {
    BorrowImm,
    BorrowMut,
    ByValue,
}

#[derive(Debug, Clone)]
pub enum Value {
    Object(ObjectValue),
    Raw(RawValueType, Vec<u8>),
    Receiving(ObjectID, SequenceNumber, Option<Type>),
}

#[derive(Debug, Clone)]
pub struct ObjectValue {
    pub type_: Type,
    pub has_public_transfer: bool,
    // true if it has been used in a public, non-entry Move call
    // In other words, false if all usages have been with non-Move commands or
    // entry Move functions
    pub used_in_non_entry_move_call: bool,
    pub contents: ObjectContents,
}

#[derive(Debug, Clone)]
pub enum ObjectContents {
    Coin(Coin),
    Raw(Vec<u8>),
}

#[derive(Debug, Clone)]
pub enum RawValueType {
    Any,
    Loaded {
        ty: Type,
        abilities: AbilitySet,
        used_in_non_entry_move_call: bool,
    },
}

#[derive(Clone, Copy)]
pub enum CommandKind<'a> {
    MoveCall {
        package: ObjectID,
        module: &'a IdentStr,
        function: &'a IdentStr,
    },
    MakeMoveVec,
    TransferObjects,
    SplitCoins,
    MergeCoins,
    Publish,
    Upgrade,
}

impl InputObjectMetadata {
    pub fn id(&self) -> ObjectID {
        match self {
            InputObjectMetadata::Receiving { id, .. } => *id,
            InputObjectMetadata::InputObject { id, .. } => *id,
        }
    }

    pub fn version(&self) -> SequenceNumber {
        match self {
            InputObjectMetadata::Receiving { version, .. } => *version,
            InputObjectMetadata::InputObject { version, .. } => *version,
        }
    }
}

impl InputValue {
    pub fn new_object(object_metadata: InputObjectMetadata, value: ObjectValue) -> Self {
        InputValue {
            object_metadata: Some(object_metadata),
            inner: ResultValue::new(Value::Object(value)),
        }
    }

    pub fn new_raw(ty: RawValueType, value: Vec<u8>) -> Self {
        InputValue {
            object_metadata: None,
            inner: ResultValue::new(Value::Raw(ty, value)),
        }
    }

    pub fn new_receiving_object(id: ObjectID, version: SequenceNumber) -> Self {
        InputValue {
            object_metadata: Some(InputObjectMetadata::Receiving { id, version }),
            inner: ResultValue::new(Value::Receiving(id, version, None)),
        }
    }
}

impl ResultValue {
    pub fn new(value: Value) -> Self {
        Self {
            last_usage_kind: None,
            value: Some(value),
        }
    }
}

impl Value {
    pub fn is_copyable(&self) -> bool {
        match self {
            Value::Object(_) => false,
            Value::Raw(RawValueType::Any, _) => true,
            Value::Raw(RawValueType::Loaded { abilities, .. }, _) => abilities.has_copy(),
            Value::Receiving(_, _, _) => false,
        }
    }

    pub fn write_bcs_bytes(&self, buf: &mut Vec<u8>) {
        match self {
            Value::Object(obj_value) => obj_value.write_bcs_bytes(buf),
            Value::Raw(_, bytes) => buf.extend(bytes),
            Value::Receiving(id, version, _) => {
                buf.extend(Receiving::new(*id, *version).to_bcs_bytes())
            }
        }
    }

    pub fn was_used_in_non_entry_move_call(&self) -> bool {
        match self {
            Value::Object(obj) => obj.used_in_non_entry_move_call,
            // Any is only used for Pure inputs, and if it was used by &mut it would have switched
            // to Loaded
            Value::Raw(RawValueType::Any, _) => false,
            Value::Raw(
                RawValueType::Loaded {
                    used_in_non_entry_move_call,
                    ..
                },
                _,
            ) => *used_in_non_entry_move_call,
            // Only thing you can do with a `Receiving<T>` is consume it, so once it's used it
            // can't be used again.
            Value::Receiving(_, _, _) => false,
        }
    }
}

impl ObjectValue {
    /// # Safety
    /// We must have the Type is the coin type, but we are unable to check it at this spot
    pub unsafe fn coin(type_: Type, coin: Coin) -> Self {
        Self {
            type_,
            has_public_transfer: true,
            used_in_non_entry_move_call: false,
            contents: ObjectContents::Coin(coin),
        }
    }

    pub fn ensure_public_transfer_eligible(&self) -> Result<(), ExecutionError> {
        if !self.has_public_transfer {
            return Err(ExecutionErrorKind::InvalidTransferObject.into());
        }
        Ok(())
    }

    pub fn write_bcs_bytes(&self, buf: &mut Vec<u8>) {
        match &self.contents {
            ObjectContents::Raw(bytes) => buf.extend(bytes),
            ObjectContents::Coin(coin) => buf.extend(coin.to_bcs_bytes()),
        }
    }
}

pub trait TryFromValue: Sized {
    fn try_from_value(value: Value) -> Result<Self, CommandArgumentError>;
}

impl TryFromValue for Value {
    fn try_from_value(value: Value) -> Result<Self, CommandArgumentError> {
        Ok(value)
    }
}

impl TryFromValue for ObjectValue {
    fn try_from_value(value: Value) -> Result<Self, CommandArgumentError> {
        match value {
            Value::Object(o) => Ok(o),
            Value::Raw(RawValueType::Any, _) => Err(CommandArgumentError::TypeMismatch),
            Value::Raw(RawValueType::Loaded { .. }, _) => Err(CommandArgumentError::TypeMismatch),
            Value::Receiving(_, _, _) => Err(CommandArgumentError::TypeMismatch),
        }
    }
}

impl TryFromValue for SuiAddress {
    fn try_from_value(value: Value) -> Result<Self, CommandArgumentError> {
        try_from_value_prim(&value, Type::Address)
    }
}

impl TryFromValue for u64 {
    fn try_from_value(value: Value) -> Result<Self, CommandArgumentError> {
        try_from_value_prim(&value, Type::U64)
    }
}

fn try_from_value_prim<'a, T: Deserialize<'a>>(
    value: &'a Value,
    expected_ty: Type,
) -> Result<T, CommandArgumentError> {
    match value {
        Value::Object(_) => Err(CommandArgumentError::TypeMismatch),
        Value::Receiving(_, _, _) => Err(CommandArgumentError::TypeMismatch),
        Value::Raw(RawValueType::Any, bytes) => {
            bcs::from_bytes(bytes).map_err(|_| CommandArgumentError::InvalidBCSBytes)
        }
        Value::Raw(RawValueType::Loaded { ty, .. }, bytes) => {
            if ty != &expected_ty {
                return Err(CommandArgumentError::TypeMismatch);
            }
            bcs::from_bytes(bytes).map_err(|_| CommandArgumentError::InvalidBCSBytes)
        }
    }
}

/// If a transaction digest shows up in this list, when executing such transaction,
/// we will always return `ExecutionError::CertificateDenied` without executing it (but still do
/// gas smashing). Because this list is not gated by protocol version, there are a few important
/// criteria for adding a digest to this list:
/// 1. The certificate must be causing all validators to either panic or hang forever deterministically.
/// 2. If we ever ship a fix to make it no longer panic or hang when executing such transaction,
/// we must make sure the transaction is already in this list. Otherwise nodes running the newer version
/// without these transactions in the list will generate forked result.
/// Below is a scenario of when we need to use this list:
/// 1. We detect that a specific transaction is causing all validators to either panic or hang forever deterministically.
/// 2. We push a CertificateDenyConfig to deny such transaction to all validators asap.
/// 3. To make sure that all fullnodes are able to sync to the latest version, we need to add the transaction digest
/// to this list as well asap, and ship this binary to all fullnodes, so that they can sync past this transaction.
/// 4. We then can start fixing the issue, and ship the fix to all nodes.
/// 5. Unfortunately, we can't remove the transaction digest from this list, because if we do so, any future
/// node that sync from genesis will fork on this transaction. We may be able to remove it once
/// we have stable snapshots and the binary has a minimum supported protocol version past the epoch.
pub fn get_denied_certificates() -> &'static HashSet<TransactionDigest> {
    static DENIED_CERTIFICATES: Lazy<HashSet<TransactionDigest>> = Lazy::new(|| HashSet::from([]));
    Lazy::force(&DENIED_CERTIFICATES)
}

pub fn is_certificate_denied(
    transaction_digest: &TransactionDigest,
    certificate_deny_set: &HashSet<TransactionDigest>,
) -> bool {
    certificate_deny_set.contains(transaction_digest)
        || get_denied_certificates().contains(transaction_digest)
}
