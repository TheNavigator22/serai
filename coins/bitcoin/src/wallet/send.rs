use std::{
  io::{self, Read},
  collections::HashMap,
};

use thiserror::Error;

use rand_core::{RngCore, CryptoRng};

use transcript::{Transcript, RecommendedTranscript};

use k256::{elliptic_curve::sec1::ToEncodedPoint, Scalar};
use frost::{curve::Secp256k1, Participant, ThresholdKeys, FrostError, sign::*};

use bitcoin::{
  hashes::Hash,
  util::sighash::{SchnorrSighashType, SighashCache, Prevouts},
  OutPoint, Script, Sequence, Witness, TxIn, TxOut, PackedLockTime, Transaction, Network, Address,
};

use crate::{
  crypto::Schnorr,
  wallet::{address, ReceivedOutput},
};

#[rustfmt::skip]
// https://github.com/bitcoin/bitcoin/blob/306ccd4927a2efe325c8d84be1bdb79edeb29b04/src/policy/policy.h#L27
const MAX_STANDARD_TX_WEIGHT: u64 = 400_000;

#[rustfmt::skip]
//https://github.com/bitcoin/bitcoin/blob/a245429d680eb95cf4c0c78e58e63e3f0f5d979a/src/test/transaction_tests.cpp#L815-L816
const DUST: u64 = 674;

#[derive(Clone, PartialEq, Eq, Debug, Error)]
pub enum TransactionError {
  #[error("no inputs were specified")]
  NoInputs,
  #[error("no outputs were created")]
  NoOutputs,
  #[error("a specified payment's amount was less than bitcoin's required minimum")]
  DustPayment,
  #[error("too much data was specified")]
  TooMuchData,
  #[error("not enough funds for these payments")]
  NotEnoughFunds,
  #[error("transaction was too large")]
  TooLargeTransaction,
}

/// A signable transaction, clone-able across attempts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SignableTransaction {
  tx: Transaction,
  offsets: Vec<Scalar>,
  prevouts: Vec<TxOut>,
  needed_fee: u64,
}

impl SignableTransaction {
  fn calculate_weight(inputs: usize, payments: &[(Address, u64)], change: Option<&Address>) -> u64 {
    // Expand this a full transaction in order to use the bitcoin library's weight function
    let mut tx = Transaction {
      version: 2,
      lock_time: PackedLockTime::ZERO,
      input: vec![
        TxIn {
          // This is a fixed size
          // See https://developer.bitcoin.org/reference/transactions.html#raw-transaction-format
          previous_output: OutPoint::default(),
          // This is empty for a Taproot spend
          script_sig: Script::new(),
          // This is fixed size, yet we do use Sequence::MAX
          sequence: Sequence::MAX,
          // Our witnesses contains a single 64-byte signature
          witness: Witness::from_vec(vec![vec![0; 64]])
        };
        inputs
      ],
      output: payments
        .iter()
        // The payment is a fixed size so we don't have to use it here
        // The script pub key is not of a fixed size and does have to be used here
        .map(|payment| TxOut { value: payment.1, script_pubkey: payment.0.script_pubkey() })
        .collect(),
    };
    if let Some(change) = change {
      // Use a 0 value since we're currently unsure what the change amount will be, and since
      // the value is fixed size (so any value could be used here)
      tx.output.push(TxOut { value: 0, script_pubkey: change.script_pubkey() });
    }
    u64::try_from(tx.weight()).unwrap()
  }

  /// Returns the fee necessary for this transaction to achieve the fee rate specified at
  /// construction.
  ///
  /// The actual fee this transaction will use is `sum(inputs) - sum(outputs)`.
  pub fn needed_fee(&self) -> u64 {
    self.needed_fee
  }

  /// Create a new SignableTransaction.
  ///
  /// If a change address is specified, any leftover funds will be sent to it if the leftover funds
  /// exceed the minimum output amount. If a change address isn't specified, all leftover funds
  /// will become part of the paid fee.
  ///
  /// If data is specified, an OP_RETURN output will be added with it.
  pub fn new(
    mut inputs: Vec<ReceivedOutput>,
    payments: &[(Address, u64)],
    change: Option<Address>,
    data: Option<Vec<u8>>,
    fee_per_weight: u64,
  ) -> Result<SignableTransaction, TransactionError> {
    if inputs.is_empty() {
      Err(TransactionError::NoInputs)?;
    }

    if payments.is_empty() && change.is_none() {
      Err(TransactionError::NoOutputs)?;
    }

    for (_, amount) in payments {
      if *amount < DUST {
        Err(TransactionError::DustPayment)?;
      }
    }

    if data.as_ref().map(|data| data.len()).unwrap_or(0) > 80 {
      Err(TransactionError::TooMuchData)?;
    }

    let input_sat = inputs.iter().map(|input| input.output.value).sum::<u64>();
    let offsets = inputs.iter().map(|input| input.offset).collect();
    let tx_ins = inputs
      .iter()
      .map(|input| TxIn {
        previous_output: input.outpoint,
        script_sig: Script::new(),
        sequence: Sequence::MAX,
        witness: Witness::new(),
      })
      .collect::<Vec<_>>();

    let payment_sat = payments.iter().map(|payment| payment.1).sum::<u64>();
    let mut tx_outs = payments
      .iter()
      .map(|payment| TxOut { value: payment.1, script_pubkey: payment.0.script_pubkey() })
      .collect::<Vec<_>>();

    // Add the OP_RETURN output
    if let Some(data) = data {
      tx_outs.push(TxOut { value: 0, script_pubkey: Script::new_op_return(&data) })
    }

    let mut weight = Self::calculate_weight(tx_ins.len(), payments, None);
    let mut needed_fee = fee_per_weight * weight;
    if input_sat < (payment_sat + needed_fee) {
      Err(TransactionError::NotEnoughFunds)?;
    }

    // If there's a change address, check if there's change to give it
    if let Some(change) = change.as_ref() {
      let weight_with_change = Self::calculate_weight(tx_ins.len(), payments, Some(change));
      let fee_with_change = fee_per_weight * weight_with_change;
      if let Some(value) = input_sat.checked_sub(payment_sat + fee_with_change) {
        if value >= DUST {
          tx_outs.push(TxOut { value, script_pubkey: change.script_pubkey() });
          weight = weight_with_change;
          needed_fee = fee_with_change;
        }
      }
    }

    if tx_outs.is_empty() {
      Err(TransactionError::NoOutputs)?;
    }

    if weight > MAX_STANDARD_TX_WEIGHT {
      Err(TransactionError::TooLargeTransaction)?;
    }

    Ok(SignableTransaction {
      tx: Transaction {
        version: 2,
        lock_time: PackedLockTime::ZERO,
        input: tx_ins,
        output: tx_outs,
      },
      offsets,
      prevouts: inputs.drain(..).map(|input| input.output).collect(),
      needed_fee,
    })
  }

  /// Create a multisig machine for this transaction.
  ///
  /// Returns None if the wrong keys are used.
  pub fn multisig(
    self,
    keys: ThresholdKeys<Secp256k1>,
    mut transcript: RecommendedTranscript,
  ) -> Option<TransactionMachine> {
    transcript.domain_separate(b"bitcoin_transaction");
    transcript.append_message(b"root_key", keys.group_key().to_encoded_point(true).as_bytes());

    // Transcript the inputs and outputs
    let tx = &self.tx;
    for input in &tx.input {
      transcript.append_message(b"input_hash", input.previous_output.txid.as_hash().into_inner());
      transcript.append_message(b"input_output_index", input.previous_output.vout.to_le_bytes());
    }
    for payment in &tx.output {
      transcript.append_message(b"output_script", payment.script_pubkey.as_bytes());
      transcript.append_message(b"output_amount", payment.value.to_le_bytes());
    }

    let mut sigs = vec![];
    for i in 0 .. tx.input.len() {
      let mut transcript = transcript.clone();
      transcript.append_message(b"signing_input", u32::try_from(i).unwrap().to_le_bytes());

      let offset = keys.clone().offset(self.offsets[i]);
      if address(Network::Bitcoin, offset.group_key())?.script_pubkey() !=
        self.prevouts[i].script_pubkey
      {
        None?;
      }

      sigs.push(AlgorithmMachine::new(
        Schnorr::new(transcript),
        keys.clone().offset(self.offsets[i]),
      ));
    }

    Some(TransactionMachine { tx: self, sigs })
  }
}

/// A FROST signing machine to produce a Bitcoin transaction.
///
/// This does not support caching its preprocess. When sign is called, the message must be empty.
/// This will panic if it isn't.
pub struct TransactionMachine {
  tx: SignableTransaction,
  sigs: Vec<AlgorithmMachine<Secp256k1, Schnorr<RecommendedTranscript>>>,
}

impl PreprocessMachine for TransactionMachine {
  type Preprocess = Vec<Preprocess<Secp256k1, ()>>;
  type Signature = Transaction;
  type SignMachine = TransactionSignMachine;

  fn preprocess<R: RngCore + CryptoRng>(
    mut self,
    rng: &mut R,
  ) -> (Self::SignMachine, Self::Preprocess) {
    let mut preprocesses = Vec::with_capacity(self.sigs.len());
    let sigs = self
      .sigs
      .drain(..)
      .map(|sig| {
        let (sig, preprocess) = sig.preprocess(rng);
        preprocesses.push(preprocess);
        sig
      })
      .collect();

    (TransactionSignMachine { tx: self.tx, sigs }, preprocesses)
  }
}

pub struct TransactionSignMachine {
  tx: SignableTransaction,
  sigs: Vec<AlgorithmSignMachine<Secp256k1, Schnorr<RecommendedTranscript>>>,
}

impl SignMachine<Transaction> for TransactionSignMachine {
  type Params = ();
  type Keys = ThresholdKeys<Secp256k1>;
  type Preprocess = Vec<Preprocess<Secp256k1, ()>>;
  type SignatureShare = Vec<SignatureShare<Secp256k1>>;
  type SignatureMachine = TransactionSignatureMachine;

  fn cache(self) -> CachedPreprocess {
    unimplemented!(
      "Bitcoin transactions don't support caching their preprocesses due to {}",
      "being already bound to a specific transaction"
    );
  }

  fn from_cache(
    _: (),
    _: ThresholdKeys<Secp256k1>,
    _: CachedPreprocess,
  ) -> Result<Self, FrostError> {
    unimplemented!(
      "Bitcoin transactions don't support caching their preprocesses due to {}",
      "being already bound to a specific transaction"
    );
  }

  fn read_preprocess<R: Read>(&self, reader: &mut R) -> io::Result<Self::Preprocess> {
    self.sigs.iter().map(|sig| sig.read_preprocess(reader)).collect()
  }

  fn sign(
    mut self,
    commitments: HashMap<Participant, Self::Preprocess>,
    msg: &[u8],
  ) -> Result<(TransactionSignatureMachine, Self::SignatureShare), FrostError> {
    if !msg.is_empty() {
      panic!("message was passed to the TransactionMachine when it generates its own");
    }

    let commitments = (0 .. self.sigs.len())
      .map(|c| {
        commitments
          .iter()
          .map(|(l, commitments)| (*l, commitments[c].clone()))
          .collect::<HashMap<_, _>>()
      })
      .collect::<Vec<_>>();

    let mut cache = SighashCache::new(&self.tx.tx);
    // Sign committing to all inputs
    let prevouts = Prevouts::All(&self.tx.prevouts);

    let mut shares = Vec::with_capacity(self.sigs.len());
    let sigs = self
      .sigs
      .drain(..)
      .enumerate()
      .map(|(i, sig)| {
        let (sig, share) = sig.sign(
          commitments[i].clone(),
          &cache
            .taproot_key_spend_signature_hash(i, &prevouts, SchnorrSighashType::Default)
            .unwrap(),
        )?;
        shares.push(share);
        Ok(sig)
      })
      .collect::<Result<_, _>>()?;

    Ok((TransactionSignatureMachine { tx: self.tx.tx, sigs }, shares))
  }
}

pub struct TransactionSignatureMachine {
  tx: Transaction,
  sigs: Vec<AlgorithmSignatureMachine<Secp256k1, Schnorr<RecommendedTranscript>>>,
}

impl SignatureMachine<Transaction> for TransactionSignatureMachine {
  type SignatureShare = Vec<SignatureShare<Secp256k1>>;

  fn read_share<R: Read>(&self, reader: &mut R) -> io::Result<Self::SignatureShare> {
    self.sigs.iter().map(|sig| sig.read_share(reader)).collect()
  }

  fn complete(
    mut self,
    mut shares: HashMap<Participant, Self::SignatureShare>,
  ) -> Result<Transaction, FrostError> {
    for (input, schnorr) in self.tx.input.iter_mut().zip(self.sigs.drain(..)) {
      let sig = schnorr.complete(
        shares.iter_mut().map(|(l, shares)| (*l, shares.remove(0))).collect::<HashMap<_, _>>(),
      )?;

      let mut witness: Witness = Witness::new();
      witness.push(sig.as_ref());
      input.witness = witness;
    }

    Ok(self.tx)
  }
}