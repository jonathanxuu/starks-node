// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

use sp_application_crypto::RuntimeAppPublic;
use codec::{Encode, Decode};
use sp_std::prelude::*;
use sp_runtime::{
    offchain::{http, Duration, storage::StorageValueRef},
    RuntimeDebug,
    traits::Member,
    transaction_validity::{
        TransactionValidity, ValidTransaction, InvalidTransaction, TransactionSource,
        TransactionPriority,
    },
};
use sp_core::crypto::KeyTypeId;
use frame_support::{
    dispatch::DispatchResult,
    decl_module, decl_event, decl_storage, Parameter, debug, decl_error, ensure,
    traits::{Get, OneSessionHandler},
};
use frame_system::{ensure_signed, ensure_none};
use frame_system::offchain::{
    SendTransactionTypes,
    SubmitTransaction,
};

#[cfg(all(feature = "std", test))]
mod mock;

#[cfg(all(feature = "std", test))]
mod tests;

/// the key type of which to sign the starks verification transactions
pub const KEY_TYPE: KeyTypeId = KeyTypeId(*b"zkst");

pub mod crypto {
    use sp_runtime::traits::Verify;
    pub mod app_sr25519 {
        pub use crate::KEY_TYPE;
        use sp_application_crypto::{app_crypto, sr25519};
        app_crypto!(sr25519, KEY_TYPE);
    }

    sp_application_crypto::with_pair! {
		/// A starks verifier keypair using sr25519 as its crypto.
		pub type AuthorityPair = app_sr25519::Pair;
	}

    /// A starks verifier signature using sr25519 as its crypto.
    pub type AuthoritySignature = app_sr25519::Signature;

    /// A starks verifier identifier using sr25519 as its crypto.
    pub type AuthorityId = app_sr25519::Public;
}

/// The status of a given verification task
#[derive(Encode, Decode, Default, PartialEq, Eq, RuntimeDebug)]
pub struct Status {
    pub verifiers: Vec<u32>,
    pub ayes: u32,
    pub nays: u32
}

#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug)]
pub struct VerificationReceipt<BlockNumber, Hash> {
    program_hash: Hash,
    // when a task is passed or not
    passed: bool,
    submit_at: BlockNumber,
    // submitted by who
    auth_index: u32,
    validators_len: u32
}

#[derive(Encode, Decode, Default, PartialEq, Eq, RuntimeDebug)]
pub struct TaskInfo {
    // the id of
    proof_id: Vec<u8>,
    inputs: Vec<u128>,
    outputs: Vec<u128>
}

/// Error which may occur while executing the off-chain code.
#[cfg_attr(test, derive(PartialEq))]
pub enum OffchainErr<BlockNumber> {
    NotValidator,
    NoTaskToExecute,
    FailToAcquireLock,
    // HttpError(10) => DeadlineReached
    // HttpError(20) => IoError
    // HttpError(0) => Unknown
    HttpError(u32),
    FailedSigning,
    SubmitTransaction(BlockNumber),
    VerificationFailed,
}

impl<BlockNumber: sp_std::fmt::Debug> sp_std::fmt::Debug for OffchainErr<BlockNumber> {
    fn fmt(&self, fmt: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
        match *self {
            OffchainErr::NotValidator => write!(fmt, "Failed to sign heartbeat"),
            OffchainErr::NoTaskToExecute => write!(fmt, "No task to Execute"),
            OffchainErr::FailToAcquireLock => write!(fmt, "Failed to acquire lock"),
            OffchainErr::HttpError(ref e) => write!(fmt, "Got an HttpError({:?})", e),
            OffchainErr::FailedSigning => write!(fmt, "Failed to sign the result"),
            OffchainErr::SubmitTransaction(ref now) =>
                write!(fmt, "Failed to submit transaction at block {:?}", now),
            OffchainErr::VerificationFailed => write!(fmt, "Failed to verify"),
        }
    }
}


const DB_PREFIX: &[u8] = b"starksnetwork/verification-tasks/";

pub type OffchainResult<T, A> = Result<A, OffchainErr<<T as frame_system::Config>::BlockNumber>>;


pub trait Config: pallet_session::Config + SendTransactionTypes<Call<Self>> {
    /// The identifier type for an offchain worker.
	type AuthorityId: Member + Parameter + RuntimeAppPublic + Default + Ord;

    type Event: From<Event<Self>> + Into<<Self as frame_system::Config>::Event>;

    /// After a task is verified, it can still be stored on chain for a `StorePeriod` of time
    type StorePeriod: Get<Self::BlockNumber>;

    /// A configuration for base priority of unsigned transactions.
	///
	/// This is exposed so that it can be tuned for particular runtime, when
	/// multiple pallets send unsigned transactions.
    type UnsignedPriority: Get<TransactionPriority>;

}


decl_event!(
	/// Events generated by the module.
    pub enum Event<T> where 
        AccountId = <T as frame_system::Config>::AccountId,
        Hash = <T as frame_system::Config>::Hash
    {
        AddVerifier(AccountId),
        RemoveVerifier(AccountId),
        TaskCreated(Hash),
    }   
);

decl_error! {
	pub enum Error for Module<T: Config> {
	    /// It's not allowed to recreated an existed task.
		TaskAlreadyExists,
		/// Only permitted verifiers can submit the result
		NotAllowed,
		/// Task does not exist
		TaskNotExists,
		/// Duplicated Submission
		DuplicatedSubmission,
    }
}

decl_storage! {
	trait Store for Module<T: Config> as StarksVerifier {

	    /// Current set of keys that are allowed to execute verification tasks
	    Keys get(fn keys): Vec<T::AuthorityId>;
		/// Affiliated params, inputs and outputs
		TaskParams get(fn task_params): map hasher(identity) T::Hash => TaskInfo;

        /// Record the verification tasks(the proofs) that are about to be verified or under verifying.
        /// the key is program hash
		OngoingTasks get(fn ongoing_tasks): map hasher(identity) T::Hash => Status;

        /// Completed proof tasks, will be stored onchain for a short period to be challenged
        SettledTasks get(fn settled_tasks): double_map
            hasher(blake2_128_concat) T::BlockNumber,
            hasher(identity) T::Hash => bool;
	}

	add_extra_genesis {
		config(keys): Vec<T::AuthorityId>;
		build(|config| Module::<T>::initialize_keys(&config.keys))
	}

}


decl_module! {
	// A public part of the pallet.
	pub struct Module<T: Config> for enum Call where origin: T::Origin {

	    type Error = Error<T>;

        const StorePeriod: T::BlockNumber = T::StorePeriod::get();

        fn deposit_event() = default;

        #[weight = 10000]
        fn create_task(origin, program_hash: T::Hash, inputs: Vec<u128>, outputs: Vec<u128>, proof_id: Vec<u8>) {
            let _who = ensure_signed(origin)?;
            // ensure task has not been created
            ensure!(!Self::task_exists(&program_hash), Error::<T>::TaskAlreadyExists);
            <TaskParams<T>>::insert(&program_hash, TaskInfo{proof_id, inputs, outputs});
            <OngoingTasks<T>>::insert(&program_hash, Status::default());
            Self::deposit_event(RawEvent::TaskCreated(program_hash));
        }

        #[weight = 10000]
        fn submit_verification(
            origin,
            receipt: VerificationReceipt<T::BlockNumber, T::Hash>,
            _signature: <T::AuthorityId as RuntimeAppPublic>::Signature,
        ) {
            ensure_none(origin)?;
            <OngoingTasks<T>>::try_mutate_exists(
                &receipt.program_hash,
                |last_status| -> DispatchResult {
                    let mut status = last_status.take().ok_or(Error::<T>::TaskNotExists)?;
                    // A verifier can not submit more than once
                    ensure!(!status.verifiers.contains(&receipt.auth_index),
                        Error::<T>::DuplicatedSubmission);

                    // > 50%
                    let threshold = (Self::authority_len() + 1) / 2;

                    if receipt.passed {
                        status.ayes += 1;
                    } else {
                        status.nays += 1;
                    }

                    let expiration = receipt.submit_at + T::StorePeriod::get();
                    // preset to None to delete the item
                    *last_status = None;

                    if status.ayes > threshold {
                        // pass the verification
                        SettledTasks::<T>::insert(expiration, receipt.program_hash, true);
                    } else if status.nays > threshold {
                         // fail the verification
                        SettledTasks::<T>::insert(expiration, receipt.program_hash, false);
                    } else {
                        // update the task status
                        *last_status = Some(status);
                    }
                    Ok(())
            })?;
          
        }


        fn offchain_worker(now: T::BlockNumber) {
            if sp_io::offchain::is_validator() {
				for res in Self::send_verification_output(now).into_iter().flatten() {
					if let Err(e) = res {
						debug::debug!(
							target: "starks-verifier",
							"Skipping verifying at {:?}: {:?}",
							now,
							e,
						)
					}
				}
			} else {
				debug::trace!(
					target: "starks-verifier",
					"Skipping verifying at {:?}. Not a validator.",
					now,
				)
			}
        }
    }
}



impl<T: Config> Module<T> {
    /// The internal entry of offchain worker
    pub(crate) fn send_verification_output(
        block_number: T::BlockNumber
    ) -> OffchainResult<T, impl Iterator<Item=OffchainResult<T, ()>>> {
        Ok(Self::local_authority_keys()
            .map( move |(auth_index, key)|
                Self::send_single_result_with_lock(auth_index, key, block_number)
        ))
    }

    fn send_single_result_with_lock(
        auth_index: u32,
        key: T::AuthorityId,
        block_number: T::BlockNumber,
    ) -> OffchainResult<T, ()> {
        let storage_key = {
            let mut prefix = DB_PREFIX.to_vec();
            prefix.extend(auth_index.encode());
            prefix
        };
        let storage = StorageValueRef::persistent(&storage_key);

        let res = storage.mutate(|tasks: Option<Option<Vec<T::Hash>>>| {
            match tasks {
                Some(Some(mut local_tasks)) =>  {
                    let task_hash = Self::task_to_execute(&mut local_tasks)?;
                    // TODO: fetch_proof, verify, and submit
                    local_tasks.push(task_hash);
                    Ok(local_tasks)
                },
                _ => {
                    let task_hash = Self::task_to_execute(&mut vec![])?;
                    Ok(vec![task_hash])
                }
            }
        })?;

        let mut local_tasks = res.map_err(|_| OffchainErr::FailToAcquireLock)?;

        // we got the lock, and do the fetch, verify, sign and send
        let res =  Self::prepare_submission(auth_index, key, block_number, *local_tasks.last().unwrap());
        
        // clear the lock in case we have failed to send transaction.
        if res.is_err() {
            local_tasks.pop();
            storage.set(&local_tasks);
        }
        res
    }


    /// fetch proof, verify, sign and submit the transaction
    fn prepare_submission(
        auth_index: u32,
        key: T::AuthorityId,
        block_number: T::BlockNumber,
        task_hash: T::Hash
    ) -> OffchainResult<T, ()> {
        
        let is_success: bool = Self::fetch_proof(&task_hash)
            .map(|proof| Self::stark_verify(&task_hash, &proof))??;
        
        let validators_len = <pallet_session::Module<T>>::validators().len() as u32;
        let receipt = VerificationReceipt {
            program_hash: task_hash,
            passed: is_success,
            submit_at: block_number,
            auth_index: auth_index,
            validators_len
        };

        let signature = key.sign(&receipt.encode()).ok_or(OffchainErr::FailedSigning)?;
        let call = Call::submit_verification(receipt, signature);
   
        debug::info!(
            target: "starks-verifier",
            "[index: {:?} report verification: {:?},  at block: {:?}]",
            auth_index,
            call,
            block_number
        );

        SubmitTransaction::<T, Call<T>>::submit_unsigned_transaction(call.into())
            .map_err(|_| OffchainErr::SubmitTransaction(block_number))?;
        
        Ok(())
    }


    fn fetch_proof(program_hash: &T::Hash) -> OffchainResult<T, Vec<u8>> {
        let proof_id = Self::task_params(program_hash).proof_id;
        let deadline = sp_io::offchain::timestamp().add(Duration::from_millis(8_000));

        let url = "https://ipfs.infura.io:5001/api/v0/cat?arg=".to_owned() + sp_std::str::from_utf8(&proof_id).unwrap();
        let request = http::Request::get(&url);
        let pending = request
            .deadline(deadline)
            .send()
            .map_err(|_| OffchainErr::HttpError(10))?;

        let response = pending.try_wait(deadline)
            .map_err(|_| OffchainErr::HttpError(20))?;

        let response = match response {
            Ok(r) => r,
            Err(_e) => return Err(OffchainErr::HttpError(20)),
        };

        // Let's check the status code before we proceed to reading the response.
        if response.code != 200 {
            debug::warn!("Unexpected status code: {}", response.code);
            return Err(OffchainErr::HttpError(0));
        }

        // Next we want to fully read the response body and collect it to a vector of bytes.
        // Note that the return object allows you to read the body in chunks as well
        // with a way to control the deadline.
        let body = response.body().collect::<Vec<u8>>();
    
        debug::native::info!("the body is {:?}", &body);

        Ok(body)
    }

    fn stark_verify(program_hash: &T::Hash, proof: &[u8]) -> OffchainResult<T, bool> {
        let TaskInfo {proof_id:_, inputs, outputs} = Self::task_params(&program_hash);

        let mut program_hash_slice = [0u8; 32];
        program_hash_slice.copy_from_slice(program_hash.as_ref());

        sp_starks::starks::verify(&program_hash_slice, &inputs, &outputs, proof)
            .map_err(|_| OffchainErr::VerificationFailed)
    }

    // return index of on-chain authorities and its corresponding local public key
    fn local_authority_keys() -> impl Iterator<Item=(u32, T::AuthorityId)> {
        // on-chain storage
        let authorities = Keys::<T>::get();
        // local keystore
        let mut local_keys = T::AuthorityId::all();

        local_keys.sort();
        authorities.into_iter()
            .enumerate()
            .filter_map(move |(index, authority)| {
                local_keys.binary_search(&authority)
                    .ok()
                    .map(|location| (index as u32, local_keys[location].clone()))
            })
    }


    // pick an on-chain tasks to execute
    fn task_to_execute(local_tasks: &mut Vec<T::Hash>) -> OffchainResult<T, T::Hash> {
        // on-chain ready-to-verify tasks
        let ongoing_tasks: Vec<T::Hash> = OngoingTasks::<T>::iter().map(|(hash, _)| hash).collect::<Vec<T::Hash>>();
        local_tasks.sort();
        // find any one task that is not executed
        // TODO： find all tasks that are not executed
        let task_hash = ongoing_tasks.into_iter()
            .enumerate().find( |(_, ot)| {
            local_tasks.binary_search(ot).is_err()
        });

        match task_hash {
            Some((_i, t)) => {
                Ok(t)
            },
            None => Err(OffchainErr::NoTaskToExecute)
        }
    }



    fn task_exists(program_hash: &T::Hash) -> bool {
        <OngoingTasks<T>>::contains_key(program_hash)
    }

    fn authority_len() -> u32 {
        Self::keys().len() as u32
    }

    fn initialize_keys(keys: &[T::AuthorityId]) {
        if !keys.is_empty() {
            assert!(Keys::<T>::get().is_empty(), "Keys are already initialized!");
            Keys::<T>::put(keys);
        }
    }

    #[cfg(test)]
    fn set_keys(keys: Vec<T::AuthorityId>) {
        Keys::<T>::put(&keys)
    }
}



impl<T: Config> sp_runtime::BoundToRuntimeAppPublic for Module<T> {
    type Public = T::AuthorityId;
}

impl<T: Config> OneSessionHandler<T::AccountId> for Module<T> {
    type Key = T::AuthorityId;

    fn on_genesis_session<'a, I: 'a>(validators: I)
        where I: Iterator<Item=(&'a T::AccountId, T::AuthorityId)>
    {
        let keys = validators.map(|x| x.1).collect::<Vec<_>>();
        Self::initialize_keys(&keys);
    }

    fn on_new_session<'a, I: 'a>(_changed: bool, validators: I, _queued_validators: I)
        where I: Iterator<Item=(&'a T::AccountId, T::AuthorityId)>
    {

        // Remember who the authorities are for the new session.
        Keys::<T>::put(validators.map(|x| x.1).collect::<Vec<_>>());
    }

    fn on_disabled(_i: usize) {
        // ignore
    }
}

/// Invalid transaction custom error. Returned when validators_len field in Receipt is incorrect.
const INVALID_VALIDATORS_LEN: u8 = 10;

impl<T: Config> frame_support::unsigned::ValidateUnsigned for Module<T> {
    type Call = Call<T>;

    fn validate_unsigned(
        _source: TransactionSource,
        call: &Self::Call
    ) -> TransactionValidity {

        if let Call::submit_verification(receipt, signature) = call {
            if receipt.submit_at > <frame_system::Module<T>>::block_number() {
                return InvalidTransaction::Future.into();
            }

            // verify that the incoming (unverified) pubkey is actually an authority id
            let keys = Keys::<T>::get();
            if keys.len() as u32 != receipt.validators_len {
                return InvalidTransaction::Custom(INVALID_VALIDATORS_LEN).into();
            }
           
            let authority_id = match keys.get(receipt.auth_index as usize) {
                Some(id) => id,
                None => return InvalidTransaction::BadProof.into(),
            };

            // check signature (this is expensive so we do it last).
            let signature_valid = receipt.using_encoded(|encoded_receipt| {
                authority_id.verify(&encoded_receipt, &signature)
            });
            
            if !signature_valid {
                return InvalidTransaction::BadProof.into();
            }

            ValidTransaction::with_tag_prefix("StarksVerifier")
                .priority(T::UnsignedPriority::get())
                .and_provides((receipt.program_hash,authority_id))
                .longevity(5)
                .propagate(true)
                .build()
        } else {
            InvalidTransaction::Call.into()
        }

    }
}