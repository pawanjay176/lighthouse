use deposit_contract::{encode_eth1_tx_data, BYTECODE, CONTRACT_DEPLOY_GAS, DEPOSIT_GAS};
use ethers_core::types::{
    transaction::{eip2718::TypedTransaction, eip2930::AccessList},
    Address, Bytes, Eip1559TransactionRequest, TransactionRequest,
};
use types::{DepositData, EthSpec, Hash256, Keypair, Signature};

/// Hardcoded deposit contract address based on sender address and nonce
pub const DEPOSIT_CONTRACT_ADDRESS: &str = "64f43BEc7F86526686C931d65362bB8698872F90";

#[derive(Debug)]
pub enum Transactions {
    Transfer(Address, Address),
    TransferLegacy(Address, Address),
    TransferAccessList(Address, Address),
    DeployDepositContract(Address),
    DepositDepositContract {
        sender: Address,
        deposit_contract_address: Address,
    },
}

/// Get a list of transactions to publish to the execution layer.
pub fn transactions<E: EthSpec>(account1: Address, account2: Address) -> Vec<TypedTransaction> {
    vec![
        Transactions::Transfer(account1, account2).transaction::<E>(),
        Transactions::TransferLegacy(account1, account2).transaction::<E>(),
        Transactions::TransferAccessList(account1, account2).transaction::<E>(),
        Transactions::DeployDepositContract(account1).transaction::<E>(),
        Transactions::DepositDepositContract {
            sender: account1,
            deposit_contract_address: ethers_core::types::Address::from_slice(
                &hex::decode(&DEPOSIT_CONTRACT_ADDRESS).unwrap(),
            ),
        }
        .transaction::<E>(),
    ]
}

impl Transactions {
    pub fn transaction<E: EthSpec>(&self) -> TypedTransaction {
        match &self {
            Self::TransferLegacy(from, to) => TransactionRequest::new()
                .from(*from)
                .to(*to)
                .value(1)
                .into(),
            Self::Transfer(from, to) => Eip1559TransactionRequest::new()
                .from(*from)
                .to(*to)
                .value(1)
                .into(),
            Self::TransferAccessList(from, to) => TransactionRequest::new()
                .from(*from)
                .to(*to)
                .value(1)
                .with_access_list(AccessList::default())
                .into(),
            Self::DeployDepositContract(addr) => TransactionRequest::new()
                .from(*addr)
                .data(Bytes::from(BYTECODE.to_vec()))
                .gas(CONTRACT_DEPLOY_GAS)
                .into(),
            Self::DepositDepositContract {
                sender,
                deposit_contract_address,
            } => {
                let keypair = Keypair::random();

                let mut deposit = DepositData {
                    pubkey: keypair.pk.into(),
                    withdrawal_credentials: Hash256::zero(),
                    amount: 32_000_000_000,
                    signature: Signature::empty().into(),
                };

                deposit.signature = deposit.create_signature(&keypair.sk, &E::default_spec());
                TransactionRequest::new()
                    .from(*sender)
                    .to(*deposit_contract_address)
                    .data(Bytes::from(encode_eth1_tx_data(&deposit).unwrap()))
                    .gas(DEPOSIT_GAS)
                    .into()
            }
        }
    }
}
