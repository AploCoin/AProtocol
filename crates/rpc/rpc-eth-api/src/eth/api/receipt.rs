//! Builds an RPC receipt response w.r.t. data layout of network.

use reth_primitives::{
    eip4844::calc_blob_gasprice,
    Address, Receipt, TransactionMeta, TransactionSigned,
    TxKind::{Call, Create},
};
use reth_rpc_types::{
    other::OtherFields, AnyReceiptEnvelope, AnyTransactionReceipt, Log, ReceiptWithBloom,
    TransactionReceipt, WithOtherFields,
};

use crate::eth::{
    api::LoadReceipt,
    cache::EthStateCache,
    error::{EthApiError, EthResult},
    EthApi,
};

impl<Provider, Pool, Network, EvmConfig> LoadReceipt for EthApi<Provider, Pool, Network, EvmConfig>
where
    Self: Send + Sync,
{
    #[inline]
    fn cache(&self) -> &EthStateCache {
        &self.inner.eth_cache
    }
}

/// Receipt response builder.
#[derive(Debug)]
pub struct ReceiptBuilder {
    /// The base response body, contains L1 fields.
    base: TransactionReceipt<AnyReceiptEnvelope<Log>>,
    /// Additional L2 fields.
    other: OtherFields,
}

impl ReceiptBuilder {
    /// Returns a new builder with the base response body (L1 fields) set.
    ///
    /// Note: This requires _all_ block receipts because we need to calculate the gas used by the
    /// transaction.
    pub fn new(
        transaction: &TransactionSigned,
        meta: TransactionMeta,
        receipt: &Receipt,
        all_receipts: &[Receipt],
    ) -> EthResult<Self> {
        // Note: we assume this transaction is valid, because it's mined (or part of pending block)
        // and we don't need to check for pre EIP-2
        let from = transaction
            .recover_signer_unchecked()
            .ok_or(EthApiError::InvalidTransactionSignature)?;

        // get the previous transaction cumulative gas used
        let gas_used = if meta.index == 0 {
            receipt.cumulative_gas_used
        } else {
            let prev_tx_idx = (meta.index - 1) as usize;
            all_receipts
                .get(prev_tx_idx)
                .map(|prev_receipt| receipt.cumulative_gas_used - prev_receipt.cumulative_gas_used)
                .unwrap_or_default()
        };

        let blob_gas_used = transaction.transaction.blob_gas_used();
        // Blob gas price should only be present if the transaction is a blob transaction
        let blob_gas_price =
            blob_gas_used.and_then(|_| meta.excess_blob_gas.map(calc_blob_gasprice));
        let logs_bloom = receipt.bloom_slow();

        // get number of logs in the block
        let mut num_logs = 0;
        for prev_receipt in all_receipts.iter().take(meta.index as usize) {
            num_logs += prev_receipt.logs.len();
        }

        let mut logs = Vec::with_capacity(receipt.logs.len());
        for (tx_log_idx, log) in receipt.logs.iter().enumerate() {
            let rpclog = Log {
                inner: log.clone(),
                block_hash: Some(meta.block_hash),
                block_number: Some(meta.block_number),
                block_timestamp: Some(meta.timestamp),
                transaction_hash: Some(meta.tx_hash),
                transaction_index: Some(meta.index),
                log_index: Some((num_logs + tx_log_idx) as u64),
                removed: false,
            };
            logs.push(rpclog);
        }

        let rpc_receipt = reth_rpc_types::Receipt {
            status: receipt.success.into(),
            cumulative_gas_used: receipt.cumulative_gas_used as u128,
            logs,
        };

        let (contract_address, to) = match transaction.transaction.kind() {
            Create => (Some(from.create(transaction.transaction.nonce())), None),
            Call(addr) => (None, Some(Address(*addr))),
        };

        #[allow(clippy::needless_update)]
        let base = TransactionReceipt {
            inner: AnyReceiptEnvelope {
                inner: ReceiptWithBloom { receipt: rpc_receipt, logs_bloom },
                r#type: transaction.transaction.tx_type().into(),
            },
            transaction_hash: meta.tx_hash,
            transaction_index: Some(meta.index),
            block_hash: Some(meta.block_hash),
            block_number: Some(meta.block_number),
            from,
            to,
            gas_used: gas_used as u128,
            contract_address,
            effective_gas_price: transaction.effective_gas_price(meta.base_fee),
            // TODO pre-byzantium receipts have a post-transaction state root
            state_root: None,
            // EIP-4844 fields
            blob_gas_price,
            blob_gas_used: blob_gas_used.map(u128::from),
        };

        Ok(Self { base, other: Default::default() })
    }

    /// Adds fields to response body.
    pub fn add_other_fields(mut self, mut fields: OtherFields) -> Self {
        self.other.append(&mut fields);
        self
    }

    /// Builds a receipt response from the base response body, and any set additional fields.
    pub fn build(self) -> AnyTransactionReceipt {
        let Self { base, other } = self;
        let mut res = WithOtherFields::new(base);
        res.other = other;

        res
    }
}
