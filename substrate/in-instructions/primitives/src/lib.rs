#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
use zeroize::Zeroize;

use scale::{Encode, Decode, MaxEncodedLen};
use scale_info::TypeInfo;

#[cfg(feature = "std")]
use serde::{Serialize, Deserialize};

use sp_application_crypto::sr25519::Signature;

#[cfg(not(feature = "std"))]
use sp_std::vec::Vec;
use sp_runtime::RuntimeDebug;

use serai_primitives::{BlockHash, Balance, NetworkId, SeraiAddress, ExternalAddress, Data};

mod shorthand;
pub use shorthand::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Encode, Decode, MaxEncodedLen, TypeInfo)]
#[cfg_attr(feature = "std", derive(Zeroize, Serialize, Deserialize))]
pub enum Application {
  DEX,
}

#[derive(Clone, PartialEq, Eq, Debug, Encode, Decode, MaxEncodedLen, TypeInfo)]
#[cfg_attr(feature = "std", derive(Zeroize, Serialize, Deserialize))]
pub struct ApplicationCall {
  application: Application,
  data: Data,
}

#[derive(Clone, PartialEq, Eq, Debug, Encode, Decode, MaxEncodedLen, TypeInfo)]
#[cfg_attr(feature = "std", derive(Zeroize, Serialize, Deserialize))]
pub enum InInstruction {
  Transfer(SeraiAddress),
  Call(ApplicationCall),
}

#[derive(Clone, PartialEq, Eq, Encode, Decode, TypeInfo, RuntimeDebug)]
#[cfg_attr(feature = "std", derive(Zeroize, Serialize, Deserialize))]
pub struct RefundableInInstruction {
  pub origin: Option<ExternalAddress>,
  pub instruction: InInstruction,
}

#[derive(Clone, PartialEq, Eq, Debug, Encode, Decode, MaxEncodedLen, TypeInfo)]
#[cfg_attr(feature = "std", derive(Zeroize, Serialize, Deserialize))]
pub struct InInstructionWithBalance {
  pub instruction: InInstruction,
  pub balance: Balance,
}

#[derive(Clone, PartialEq, Eq, Encode, Decode, TypeInfo, RuntimeDebug)]
#[cfg_attr(feature = "std", derive(Zeroize, Serialize, Deserialize))]
pub struct Batch {
  pub network: NetworkId,
  pub id: u32,
  pub block: BlockHash,
  pub instructions: Vec<InInstructionWithBalance>,
}

#[derive(Clone, PartialEq, Eq, Encode, Decode, TypeInfo, RuntimeDebug)]
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
pub struct SignedBatch {
  pub batch: Batch,
  pub signature: Signature,
}

#[cfg(feature = "std")]
impl Zeroize for SignedBatch {
  fn zeroize(&mut self) {
    self.batch.zeroize();
    self.signature.as_mut().zeroize();
  }
}