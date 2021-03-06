use log::{debug, info};
use rand::{thread_rng, Rng};
use std::collections::{BTreeMap, HashMap, HashSet};

use witnet_crypto::hash::calculate_sha256;

use super::{
    chain::{
        transaction_tag, CommitOutput, DataRequestOutput, DataRequestReport, DataRequestStage,
        DataRequestState, Epoch, Hash, Hashable, Input, Output, OutputPointer, PublicKeyHash,
        RevealOutput, TallyOutput, Transaction, TransactionBody, TransactionType,
        ValueTransferOutput,
    },
    error::DataRequestError,
};

use serde::{Deserialize, Serialize};
/// Pool of active data requests
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DataRequestPool {
    /// Current active data request, in which this node has announced commitments.
    /// Key: Data Request Pointer, Value: Reveal Transaction
    pub waiting_for_reveal: HashMap<OutputPointer, Transaction>,
    /// List of active data request output pointers ordered by epoch (for mining purposes)
    pub data_requests_by_epoch: BTreeMap<Epoch, HashSet<OutputPointer>>,
    /// List of active data requests indexed by output pointer
    pub data_request_pool: HashMap<OutputPointer, DataRequestState>,
    /// List of data requests that should be persisted into storage
    pub to_be_stored: Vec<(OutputPointer, DataRequestReport)>,
}

impl DataRequestPool {
    /// Get all available data requests output pointers for an epoch
    pub fn get_dr_output_pointers_by_epoch(&self, epoch: Epoch) -> Vec<OutputPointer> {
        let range = 0..=epoch;
        self.data_requests_by_epoch
            .range(range)
            .flat_map(|(_epoch, hashset)| hashset.iter().cloned())
            .collect()
    }

    /// Get a `DataRequestOuput` for a `OutputPointer`
    pub fn get_dr_output(&self, output_pointer: &OutputPointer) -> Option<DataRequestOutput> {
        self.data_request_pool
            .get(output_pointer)
            .map(|dr_state| dr_state.data_request.clone())
    }

    /// Get all reveals related to a `DataRequestOuput` for a `OutputPointer`
    pub fn get_reveals(&self, output_pointer: &OutputPointer) -> Option<Vec<RevealOutput>> {
        self.data_request_pool
            .get(output_pointer)
            .map(|dr_state| dr_state.info.reveals.values().cloned().collect())
    }

    /// Insert a reveal transaction into the pool
    pub fn insert_reveal(&mut self, data_request_pointer: OutputPointer, reveal: Transaction) {
        self.waiting_for_reveal.insert(data_request_pointer, reveal);
    }

    /// Get all the reveals
    pub fn get_all_reveals(&self) -> HashMap<OutputPointer, Vec<RevealOutput>> {
        self.data_request_pool
            .iter()
            .filter_map(|(dr_pointer, dr_state)| {
                if let DataRequestStage::TALLY = dr_state.stage {
                    let reveals = dr_state.info.reveals.values().cloned().collect();
                    Some((dr_pointer.clone(), reveals))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Add a data request to the data request pool
    pub fn add_data_request(
        &mut self,
        epoch: Epoch,
        output_pointer: OutputPointer,
        data_request: DataRequestOutput,
    ) {
        let dr_state = DataRequestState::new(data_request, epoch);

        self.data_requests_by_epoch
            .entry(epoch)
            .or_insert_with(HashSet::new)
            .insert(output_pointer.clone());
        self.data_request_pool.insert(output_pointer, dr_state);
    }

    /// Our node made a commitment and is waiting for it to be included in a block.
    /// When that happens, it should send the reveal transaction.
    /// Returns the old reveal for this data request, if it exists, on success.
    /// On failure returns the reveal transaction back.
    #[allow(unused)]
    pub fn add_own_reveal(
        &mut self,
        data_request_pointer: OutputPointer,
        reveal: Transaction,
    ) -> Result<Option<Transaction>, Transaction> {
        // TODO: this checks could be avoided if instead of `Transaction` we accept a
        // `RevealTransaction`, which is defined as
        // struct RevealTransaction(Transaction)
        // but can only be constructed using a method which checks the validity
        // The reveal transaction can only have one input and one output
        if reveal.body.inputs.len() == 1 && reveal.body.outputs.len() == 1 {
            // Input: CommitInput, Output: RevealOutput
            match reveal.body.outputs[0] {
                Output::Reveal(..) => {
                    Ok(self.waiting_for_reveal.insert(data_request_pointer, reveal))
                }
                _ => Err(reveal),
            }
        } else {
            Err(reveal)
        }
    }

    /// Add a commit to the corresponding data request
    fn add_commit(
        &mut self,
        pkh: PublicKeyHash,
        commit_input: &Input,
        commit_output: &CommitOutput,
        tx_hash: &Hash,
        block_hash: &Hash,
    ) -> Result<(), failure::Error> {
        // For a commit output, we need to get the corresponding data request input
        let dr_pointer = commit_input.output_pointer();
        // The data request must be from a previous block, and must not be timelocked.
        // This is not checked here, as it should have made the block invalid.
        if let Some(dr) = self.data_request_pool.get_mut(&dr_pointer) {
            dr.add_commit(pkh, commit_output.clone())?
        } else {
            Err(DataRequestError::AddCommitFail {
                block_hash: *block_hash,
                tx_hash: *tx_hash,
                commit_output: commit_output.clone(),
                dr_pointer,
            })?
        }

        Ok(())
    }

    /// Add a reveal transaction
    fn add_reveal(
        &mut self,
        pkh: PublicKeyHash,
        reveal_input: &Input,
        reveal_output: &RevealOutput,
        tx_hash: &Hash,
        block_hash: &Hash,
    ) -> Result<(), failure::Error> {
        // For a commit output, we need to get the corresponding data request input
        let dr_pointer = reveal_input.output_pointer();
        // The data request must be from a previous block, and must not be timelocked.
        // This is not checked here, as it should have made the block invalid.
        if let Some(dr) = self.data_request_pool.get_mut(&dr_pointer) {
            dr.add_reveal(pkh, reveal_output.clone())?
        } else {
            Err(DataRequestError::AddRevealFail {
                block_hash: *block_hash,
                tx_hash: *tx_hash,
                reveal_output: reveal_output.clone(),
                dr_pointer,
            })?
        }

        Ok(())
    }

    /// Add a tally transaction
    #[allow(clippy::needless_pass_by_value)]
    fn add_tally(
        &mut self,
        tally_input: &Input,
        tally_pointer: OutputPointer,
        block_hash: &Hash,
    ) -> Result<(), failure::Error> {
        let dr_pointer = tally_input.output_pointer();

        let (_dr, dr_info) = Self::resolve_data_request(
            &mut self.data_request_pool,
            &dr_pointer,
            tally_pointer.clone(),
            block_hash,
        )?;

        // Since this method does not have access to the storage, we save the
        // "to be stored" inside a vector and provide another method to store them
        self.to_be_stored.push((dr_pointer, dr_info.clone()));

        Ok(())
    }

    /// Removes a resolved data request from the data request pool, returning the `DataRequestOutput`
    /// and a `DataRequestInfoStorage` which should be persisted into storage.
    fn resolve_data_request(
        data_request_pool: &mut HashMap<OutputPointer, DataRequestState>,
        dr_pointer: &OutputPointer,
        tally_pointer: OutputPointer,
        block_hash: &Hash,
    ) -> Result<(DataRequestOutput, DataRequestReport), failure::Error> {
        let transaction_id = tally_pointer.transaction_id;

        let dr_state: Result<DataRequestState, failure::Error> =
            data_request_pool.remove(dr_pointer).ok_or_else(|| {
                DataRequestError::AddTallyFail {
                    block_hash: *block_hash,
                    tx_hash: transaction_id,
                    tally_pointer: tally_pointer.clone(),
                    dr_pointer: dr_pointer.clone(),
                }
                .into()
            });
        let dr_state = dr_state?;

        dr_state.add_tally(tally_pointer)
    }

    /// Return the list of data requests in which this node has participated and are ready
    /// for reveal (the node should send a reveal transaction).
    /// This function must be called after `add_data_requests_from_block`, in order to update
    /// the stage of all the data requests.
    pub fn update_data_request_stages(&mut self) -> Vec<Transaction> {
        let waiting_for_reveal = &mut self.waiting_for_reveal;
        let data_requests_by_epoch = &mut self.data_requests_by_epoch;
        // Update the stage of the active data requests
        self.data_request_pool
            .iter_mut()
            .filter_map(|(dr_pointer, dr_state)| {
                // We can notify the user that a data request from "my_claims" is available
                // for reveal.
                if dr_state.update_stage() {
                    if let DataRequestStage::REVEAL = dr_state.stage {
                        // When a data request changes from commit stage to reveal stage, it should
                        // be removed from the "data_requests_by_epoch" map, which stores the data
                        // requests potentially available for commitment
                        if let Some(hs) = data_requests_by_epoch.get_mut(&dr_state.epoch) {
                            let present = hs.remove(dr_pointer);
                            if hs.is_empty() {
                                data_requests_by_epoch.remove(&dr_state.epoch);
                            }
                            if !present {
                                log::error!(
                                    "Data request {:?} was not present in the \
                                     data_requests_by_epoch map (epoch #{})",
                                    dr_pointer,
                                    dr_state.epoch
                                );
                            }
                        }

                        if let Some(transaction) = waiting_for_reveal.remove(dr_pointer) {
                            // We submitted a commit for this data request!
                            // But has it been included into the block?
                            let pkh = PublicKeyHash::from_public_key(
                                &transaction.signatures[0].public_key,
                            );
                            if dr_state.info.commits.contains_key(&pkh) {
                                // We found our commit, return the reveal transaction to be sent
                                return Some(transaction);
                            } else {
                                info!(
                                    "The sent commit transaction has not been \
                                     selected to be part of the data request {:?}",
                                    dr_pointer
                                );
                                debug!(
                                    "Commit with pkh ({}) removed from the list of commits waiting \
                                     for reveal",
                                    pkh
                                );
                            }
                        }
                    }
                }

                None
            })
            .collect()
    }

    /// Process a transaction from a block and update the data request pool accordingly:
    /// * New data requests are inserted and wait for commitments
    /// * New commitments are added to their respective data requests, updating the stage to reveal
    /// * New reveals are added to their respective data requests, updating the stage to tally
    /// The epoch is needed as the key to the available data requests map
    /// The block hash is only used for debugging purposes
    pub fn process_transaction(
        &mut self,
        t: &Transaction,
        epoch: Epoch,
        block_hash: &Hash,
    ) -> Result<(), failure::Error> {
        let tx_hash = t.hash();
        match transaction_tag(&t.body) {
            TransactionType::Commit => {
                let pkh = PublicKeyHash::from_public_key(&t.signatures[0].public_key);
                let commit_input = &t.body.inputs[0];
                if let Output::Commit(commit_output) = &t.body.outputs[0] {
                    self.add_commit(pkh, commit_input, commit_output, &tx_hash, block_hash)?
                }
            }
            TransactionType::Reveal => {
                let pkh = PublicKeyHash::from_public_key(&t.signatures[0].public_key);
                let reveal_input = &t.body.inputs[0];
                if let Output::Reveal(reveal_output) = &t.body.outputs[0] {
                    self.add_reveal(pkh, reveal_input, reveal_output, &tx_hash, block_hash)?
                }
            }
            TransactionType::DataRequest => {
                for (i, s) in t.body.outputs.iter().enumerate() {
                    if let Output::DataRequest(dr) = s {
                        let output_index = i as u32;
                        let pointer = OutputPointer {
                            transaction_id: tx_hash,
                            output_index,
                        };
                        // A data request output should have a valid value transfer input
                        // Which we assume valid as it should have been already verified
                        // time_lock_epoch: The epoch during which we will start accepting
                        // commitments for this data request
                        // FIXME(#338): implement time lock
                        // An enhancement to the epoch manager would be a handler GetState which returns
                        // the needed constants to calculate the current epoch. This way we avoid all the
                        // calls to GetEpoch
                        let time_lock_epoch = 0;
                        let dr_epoch = std::cmp::max(epoch, time_lock_epoch);
                        self.add_data_request(dr_epoch, pointer.clone(), dr.clone());
                    }
                }
            }
            TransactionType::Tally => {
                let tally_input = &t.body.inputs[0];
                let tally_index = t.body.outputs.len() - 1;

                let tally_pointer = OutputPointer {
                    transaction_id: tx_hash,
                    output_index: tally_index as u32,
                };
                self.add_tally(tally_input, tally_pointer, block_hash)?
            }
            _ => {}
        }

        Ok(())
    }

    /// Get the detailed state of a data request.
    #[allow(unused)]
    pub fn data_request_state(
        &self,
        data_request_pointer: &OutputPointer,
    ) -> Option<&DataRequestState> {
        self.data_request_pool.get(data_request_pointer)
    }

    /// Get the data request info of the finished data requests, to be persisted to the storage
    #[allow(unused)]
    pub fn finished_data_requests(&mut self) -> Vec<(OutputPointer, DataRequestReport)> {
        std::mem::replace(&mut self.to_be_stored, vec![])
    }
}

/// Function to calculate the commit reward
pub fn calculate_commit_reward(dr_output: &DataRequestOutput) -> u64 {
    dr_output.value / u64::from(dr_output.witnesses) - dr_output.commit_fee
}

/// Function to calculate the reveal reward
pub fn calculate_reveal_reward(dr_output: &DataRequestOutput) -> u64 {
    calculate_commit_reward(dr_output) - dr_output.reveal_fee
}

/// Function to calculate the value transfer reward
pub fn calculate_dr_vt_reward(dr_output: &DataRequestOutput) -> u64 {
    calculate_reveal_reward(dr_output) - dr_output.tally_fee
}

/// Function to calculate the tally change
pub fn calculate_tally_change(dr_output: &DataRequestOutput, n_reveals: u64) -> u64 {
    calculate_reveal_reward(dr_output) * (u64::from(dr_output.witnesses) - n_reveals)
}

/// Create data request commitment
pub fn create_commit_body(
    dr_pointer: OutputPointer,
    dr_output: &DataRequestOutput,
    reveal: Vec<u8>,
) -> TransactionBody {
    // Create input
    let commit_input = Input::new(dr_pointer);

    // Calculate reveal_value
    let commit_value = calculate_commit_reward(&dr_output);

    // TODO: Remove nonce after VRF implementation
    let nonce: u8 = thread_rng().gen();
    let mut v = vec![];
    v.extend(&[nonce]);
    v.extend(reveal.as_slice());
    let reveal_hash = calculate_sha256(&v).into();

    // Create output
    let commit_output = Output::Commit(CommitOutput {
        commitment: reveal_hash,
        value: commit_value,
    });

    TransactionBody::new(0, vec![commit_input], vec![commit_output])
}

/// Create data request reveal
pub fn create_reveal_body(
    dr_pointer: OutputPointer,
    dr_output: &DataRequestOutput,
    reveal: Vec<u8>,
    pkh: PublicKeyHash,
) -> TransactionBody {
    // Create input
    let reveal_input = Input::new(dr_pointer);

    // Calculate reveal_value
    let reveal_value = calculate_reveal_reward(&dr_output);

    // Create output
    let reveal_output = Output::Reveal(RevealOutput {
        reveal,
        pkh,
        value: reveal_value,
    });

    TransactionBody::new(0, vec![reveal_input], vec![reveal_output])
}

pub fn create_vt_tally(
    dr_pointer: OutputPointer,
    dr_output: &DataRequestOutput,
    reveals: Vec<RevealOutput>,
) -> (Vec<Input>, Vec<Output>, Vec<Vec<u8>>) {
    let mut outputs = vec![];
    let mut results = vec![];
    // TODO: Do not reward dishonest witnesses
    let reveal_reward = calculate_dr_vt_reward(dr_output);

    for reveal in reveals {
        let vt_output = ValueTransferOutput {
            pkh: reveal.pkh,
            value: reveal_reward,
        };
        outputs.push(Output::ValueTransfer(vt_output));

        results.push(reveal.reveal);
    }

    let inputs = vec![Input::new(dr_pointer)];

    (inputs, outputs, results)
}

pub fn create_tally_body(
    dr_output: &DataRequestOutput,
    inputs: Vec<Input>,
    mut outputs: Vec<Output>,
    consensus: Vec<u8>,
) -> TransactionBody {
    let change = calculate_tally_change(dr_output, outputs.len() as u64);
    let pkh = dr_output.pkh;

    let tally_output = TallyOutput {
        result: consensus,
        pkh,
        value: change,
    };
    outputs.push(Output::Tally(tally_output));
    TransactionBody::new(0, inputs, outputs)
}

#[cfg(test)]
mod tests {
    use crate::{chain::*, data_request::DataRequestPool};

    fn fake_transaction_zip(z: Vec<(Input, Output)>) -> Transaction {
        let (inputs, outputs) = z.into_iter().unzip();

        Transaction::new(
            TransactionBody::new(0, inputs, outputs),
            vec![KeyedSignature::default()],
        )
    }

    fn add_data_requests() -> (u32, Hash, DataRequestPool, OutputPointer) {
        let fake_block_hash = Hash::SHA256([1; 32]);
        let epoch = 0;
        let data_request = DataRequestOutput::default();
        let empty_info = DataRequestInfo::default();
        let transaction = fake_transaction_zip(vec![(
            Input::default(),
            Output::DataRequest(data_request.clone()),
        )]);
        let dr_pointer = OutputPointer {
            transaction_id: transaction.hash(),
            output_index: 0,
        };

        let mut p = DataRequestPool::default();
        let _aux = p.process_transaction(&transaction, epoch, &fake_block_hash);

        assert!(p.waiting_for_reveal.is_empty());
        assert!(p.data_requests_by_epoch[&epoch].contains(&dr_pointer));
        assert_eq!(p.data_request_pool[&dr_pointer].data_request, data_request);
        assert_eq!(p.data_request_pool[&dr_pointer].info, empty_info);
        assert_eq!(
            p.data_request_pool[&dr_pointer].stage,
            DataRequestStage::COMMIT
        );
        assert!(p.to_be_stored.is_empty());

        assert!(p.update_data_request_stages().is_empty());

        (epoch, fake_block_hash, p, dr_pointer)
    }

    fn from_commit_to_reveal(
        epoch: u32,
        fake_block_hash: Hash,
        mut p: DataRequestPool,
        dr_pointer: OutputPointer,
    ) -> (u32, Hash, DataRequestPool, OutputPointer, CommitOutput) {
        let commit_output = CommitOutput::default();
        let commit_transaction = fake_transaction_zip(vec![(
            Input::new(dr_pointer.clone()),
            Output::Commit(commit_output.clone()),
        )]);

        let _aux = p.process_transaction(&commit_transaction, epoch + 1, &fake_block_hash);

        // And we can also get all the commit pointers from the data request
        assert_eq!(
            p.data_request_pool[&dr_pointer]
                .info
                .commits
                .values()
                .collect::<Vec<_>>(),
            vec![&commit_output],
        );

        // Still in commit stage until we update
        assert_eq!(
            p.data_request_pool[&dr_pointer].stage,
            DataRequestStage::COMMIT
        );

        assert!(p.data_requests_by_epoch[&epoch].contains(&dr_pointer));

        // Update stages
        assert!(p.update_data_request_stages().is_empty());

        // Now in reveal stage
        assert_eq!(
            p.data_request_pool[&dr_pointer].stage,
            DataRequestStage::REVEAL
        );

        // The data request was removed from the data_requests_by_epoch map
        assert!(!p
            .data_requests_by_epoch
            .get(&epoch)
            .map(|x| x.contains(&dr_pointer))
            .unwrap_or(false));

        (epoch, fake_block_hash, p, dr_pointer, commit_output)
    }

    fn from_reveal_to_tally(
        epoch: u32,
        fake_block_hash: Hash,
        mut p: DataRequestPool,
        dr_pointer: OutputPointer,
        commit_output: CommitOutput,
    ) -> (u32, Hash, DataRequestPool, OutputPointer) {
        let reveal_output = RevealOutput::default();
        let reveal_transaction = fake_transaction_zip(vec![(
            Input::new(dr_pointer.clone()),
            Output::Reveal(reveal_output.clone()),
        )]);

        let _aux = p.process_transaction(&reveal_transaction, epoch + 2, &fake_block_hash);

        // And we can also get all the commit/reveal pointers from the data request
        assert_eq!(
            p.data_request_pool[&dr_pointer]
                .info
                .commits
                .values()
                .collect::<Vec<_>>(),
            vec![&commit_output],
        );
        assert_eq!(
            p.data_request_pool[&dr_pointer]
                .info
                .reveals
                .values()
                .collect::<Vec<_>>(),
            vec![&reveal_output],
        );

        // Still in reveal stage until we update
        assert_eq!(
            p.data_request_pool[&dr_pointer].stage,
            DataRequestStage::REVEAL
        );

        // Update stages
        assert!(p.update_data_request_stages().is_empty());

        // Now in tally stage
        assert_eq!(
            p.data_request_pool[&dr_pointer].stage,
            DataRequestStage::TALLY
        );

        (epoch, fake_block_hash, p, dr_pointer)
    }

    fn from_tally_to_storage(
        epoch: u32,
        fake_block_hash: Hash,
        mut p: DataRequestPool,
        dr_pointer: OutputPointer,
    ) {
        let tally_output = TallyOutput::default();
        let mut tally_transaction = fake_transaction_zip(vec![(
            Input::new(dr_pointer.clone()),
            Output::ValueTransfer(ValueTransferOutput::default()),
        )]);
        tally_transaction
            .body
            .outputs
            .push(Output::Tally(tally_output.clone()));

        // There is nothing to be stored yet
        assert_eq!(p.to_be_stored.len(), 0);

        // Process tally: this will remove the data request from the pool
        let _aux = p.process_transaction(&tally_transaction, epoch + 2, &fake_block_hash);

        // And the data request has been removed from the pool
        assert_eq!(p.data_request_pool.get(&dr_pointer), None);

        // Update stages
        assert!(p.update_data_request_stages().is_empty());

        assert_eq!(p.to_be_stored.len(), 1);
        assert_eq!(p.to_be_stored[0].0, dr_pointer);
    }

    #[test]
    fn test_add_data_requests() {
        add_data_requests();
    }

    #[test]
    fn test_from_commit_to_reveal() {
        let (epoch, fake_block_hash, p, dr_pointer) = add_data_requests();

        from_commit_to_reveal(epoch, fake_block_hash, p, dr_pointer);
    }

    #[test]
    fn test_from_reveal_to_tally() {
        let (epoch, fake_block_hash, p, dr_pointer) = add_data_requests();
        let (epoch, fake_block_hash, p, dr_pointer, commit_output) =
            from_commit_to_reveal(epoch, fake_block_hash, p, dr_pointer);

        from_reveal_to_tally(epoch, fake_block_hash, p, dr_pointer, commit_output);
    }

    #[test]
    fn test_from_tally_to_storage() {
        let (epoch, fake_block_hash, p, dr_pointer) = add_data_requests();
        let (epoch, fake_block_hash, p, dr_pointer, commit_output) =
            from_commit_to_reveal(epoch, fake_block_hash, p, dr_pointer);
        let (epoch, fake_block_hash, p, dr_pointer) =
            from_reveal_to_tally(epoch, fake_block_hash, p, dr_pointer, commit_output);

        from_tally_to_storage(epoch, fake_block_hash, p, dr_pointer);
    }

    #[test]
    fn my_claims() {
        // Test the `add_own_reveal` function
        let (epoch, fake_block_hash, mut p, dr_pointer) = add_data_requests();

        let commit_output = CommitOutput::default();
        let commit_transaction = fake_transaction_zip(vec![(
            Input::new(dr_pointer.clone()),
            Output::Commit(commit_output.clone()),
        )]);

        let reveal_output = RevealOutput::default();
        let reveal_transaction = fake_transaction_zip(vec![(
            Input::new(dr_pointer.clone()),
            Output::Reveal(reveal_output.clone()),
        )]);

        // Add reveal transaction for this commit, will be returned by the update_data_request_stages
        // function when the data request is in reveal stage
        p.add_own_reveal(dr_pointer.clone(), reveal_transaction.clone())
            .unwrap();

        assert_eq!(
            p.waiting_for_reveal.get(&dr_pointer),
            Some(&reveal_transaction)
        );

        let _aux = p.process_transaction(&commit_transaction, epoch + 1, &fake_block_hash);

        // Still in commit stage until we update
        assert_eq!(
            p.data_request_pool[&dr_pointer].stage,
            DataRequestStage::COMMIT
        );

        // Update stages. This will return our reveal transaction
        let my_reveals = p.update_data_request_stages();
        assert_eq!(my_reveals.len(), 1);
        let my_reveal = &my_reveals[0];
        assert_eq!(my_reveal, &reveal_transaction);
        assert_eq!(p.waiting_for_reveal.get(&dr_pointer), None);

        // Now in reveal stage
        assert_eq!(
            p.data_request_pool[&dr_pointer].stage,
            DataRequestStage::REVEAL
        );

        from_reveal_to_tally(epoch, fake_block_hash, p, dr_pointer, commit_output);
    }

    #[test]
    fn update_multiple_times() {
        // Only the first consecutive call to update_data_request_stages should change the state
        let (epoch, fake_block_hash, mut p, dr_pointer) = add_data_requests();

        assert!(p.update_data_request_stages().is_empty());

        assert_eq!(
            p.data_request_pool[&dr_pointer].stage,
            DataRequestStage::COMMIT
        );

        let (epoch, fake_block_hash, mut p, dr_pointer, commit_output) =
            from_commit_to_reveal(epoch, fake_block_hash, p, dr_pointer);

        // Update stages
        assert!(p.update_data_request_stages().is_empty());

        // Now in reveal stage
        assert_eq!(
            p.data_request_pool[&dr_pointer].stage,
            DataRequestStage::REVEAL
        );

        let (epoch, fake_block_hash, mut p, dr_pointer) =
            from_reveal_to_tally(epoch, fake_block_hash, p, dr_pointer, commit_output);

        // Update stages
        assert!(p.update_data_request_stages().is_empty());

        // Now in tally stage
        assert_eq!(
            p.data_request_pool[&dr_pointer].stage,
            DataRequestStage::TALLY
        );

        from_tally_to_storage(epoch, fake_block_hash, p, dr_pointer);
    }
}
