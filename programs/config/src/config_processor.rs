//! Config program

use crate::ConfigKeys;
use bincode::deserialize;
use solana_sdk::{
    account::{ReadableAccount, WritableAccount},
    feature_set, ic_msg,
    instruction::InstructionError,
    keyed_account::keyed_account_at_index,
    process_instruction::InvokeContext,
    program_utils::limited_deserialize,
    pubkey::Pubkey,
};

pub fn process_instruction(
    _program_id: &Pubkey,
    data: &[u8],
    invoke_context: &mut dyn InvokeContext,
) -> Result<(), InstructionError> {
    let keyed_accounts = invoke_context.get_keyed_accounts()?;

    let key_list: ConfigKeys = limited_deserialize(data)?;
    let config_keyed_account = &mut keyed_account_at_index(keyed_accounts, 0)?;
    let current_data: ConfigKeys = {
        let config_account = config_keyed_account.try_account_ref_mut()?;
        if invoke_context.is_feature_active(&feature_set::check_program_owner::id())
            && config_account.owner != crate::id()
        {
            return Err(InstructionError::InvalidAccountOwner);
        }

        deserialize(&config_account.data()).map_err(|err| {
            ic_msg!(
                invoke_context,
                "Unable to deserialize config account: {}",
                err
            );
            InstructionError::InvalidAccountData
        })?
    };
    let current_signer_keys: Vec<Pubkey> = current_data
        .keys
        .iter()
        .filter(|(_, is_signer)| *is_signer)
        .map(|(pubkey, _)| *pubkey)
        .collect();

    if current_signer_keys.is_empty() {
        // Config account keypair must be a signer on account initialization,
        // or when no signers specified in Config data
        if config_keyed_account.signer_key().is_none() {
            return Err(InstructionError::MissingRequiredSignature);
        }
    }

    let mut counter = 0;
    let mut keyed_accounts_iter = keyed_accounts.iter().skip(1);
    for (signer, _) in key_list.keys.iter().filter(|(_, is_signer)| *is_signer) {
        counter += 1;
        if signer != config_keyed_account.unsigned_key() {
            let signer_account = keyed_accounts_iter.next();
            if signer_account.is_none() {
                ic_msg!(
                    invoke_context,
                    "account {:?} is not in account list",
                    signer
                );
                return Err(InstructionError::MissingRequiredSignature);
            }
            let signer_key = signer_account.unwrap().signer_key();
            if signer_key.is_none() {
                ic_msg!(
                    invoke_context,
                    "account {:?} signer_key().is_none()",
                    signer
                );
                return Err(InstructionError::MissingRequiredSignature);
            }
            if signer_key.unwrap() != signer {
                ic_msg!(
                    invoke_context,
                    "account[{:?}].signer_key() does not match Config data)",
                    counter + 1
                );
                return Err(InstructionError::MissingRequiredSignature);
            }
            // If Config account is already initialized, update signatures must match Config data
            if !current_data.keys.is_empty()
                && !current_signer_keys.iter().any(|pubkey| pubkey == signer)
            {
                ic_msg!(
                    invoke_context,
                    "account {:?} is not in stored signer list",
                    signer
                );
                return Err(InstructionError::MissingRequiredSignature);
            }
        } else if config_keyed_account.signer_key().is_none() {
            ic_msg!(invoke_context, "account[0].signer_key().is_none()");
            return Err(InstructionError::MissingRequiredSignature);
        }
    }

    // Check for Config data signers not present in incoming account update
    if current_signer_keys.len() > counter {
        ic_msg!(
            invoke_context,
            "too few signers: {:?}; expected: {:?}",
            counter,
            current_signer_keys.len()
        );
        return Err(InstructionError::MissingRequiredSignature);
    }

    if config_keyed_account.data_len()? < data.len() {
        ic_msg!(invoke_context, "instruction data too large");
        return Err(InstructionError::InvalidInstructionData);
    }

    config_keyed_account
        .try_account_ref_mut()?
        .data_as_mut_slice()[..data.len()]
        .copy_from_slice(&data);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config_instruction, get_config_data, id, ConfigKeys, ConfigState};
    use bincode::serialized_size;
    use serde_derive::{Deserialize, Serialize};
    use solana_sdk::{
        account::{Account, AccountSharedData},
        keyed_account::create_keyed_accounts_unified,
        process_instruction::MockInvokeContext,
        signature::{Keypair, Signer},
        system_instruction::SystemInstruction,
    };
    use std::cell::RefCell;

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct MyConfig {
        pub item: u64,
    }
    impl Default for MyConfig {
        fn default() -> Self {
            Self { item: 123_456_789 }
        }
    }
    impl MyConfig {
        pub fn new(item: u64) -> Self {
            Self { item }
        }
        pub fn deserialize(input: &[u8]) -> Option<Self> {
            deserialize(input).ok()
        }
    }

    impl ConfigState for MyConfig {
        fn max_space() -> u64 {
            serialized_size(&Self::default()).unwrap()
        }
    }

    fn create_config_account(keys: Vec<(Pubkey, bool)>) -> (Keypair, RefCell<AccountSharedData>) {
        let from_pubkey = solana_sdk::pubkey::new_rand();
        let config_keypair = Keypair::new();
        let config_pubkey = config_keypair.pubkey();

        let instructions =
            config_instruction::create_account::<MyConfig>(&from_pubkey, &config_pubkey, 1, keys);

        let system_instruction = limited_deserialize(&instructions[0].data).unwrap();
        let space = match system_instruction {
            SystemInstruction::CreateAccount {
                lamports: _,
                space,
                owner: _,
            } => space,
            _ => panic!("Not a CreateAccount system instruction"),
        };
        let config_account = RefCell::new(AccountSharedData::from(Account {
            data: vec![0; space as usize],
            owner: id(),
            ..Account::default()
        }));
        let accounts = vec![(true, false, &config_pubkey, &config_account)];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instructions[1].data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(())
        );

        (config_keypair, config_account)
    }

    #[test]
    fn test_process_create_ok() {
        solana_logger::setup();
        let keys = vec![];
        let (_, config_account) = create_config_account(keys);
        assert_eq!(
            Some(MyConfig::default()),
            deserialize(get_config_data(&config_account.borrow().data()).unwrap()).ok()
        );
    }

    #[test]
    fn test_process_store_ok() {
        solana_logger::setup();
        let keys = vec![];
        let (config_keypair, config_account) = create_config_account(keys.clone());
        let config_pubkey = config_keypair.pubkey();
        let my_config = MyConfig::new(42);

        let instruction = config_instruction::store(&config_pubkey, true, keys, &my_config);
        let accounts = vec![(true, false, &config_pubkey, &config_account)];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(())
        );
        assert_eq!(
            Some(my_config),
            deserialize(get_config_data(&config_account.borrow().data()).unwrap()).ok()
        );
    }

    #[test]
    fn test_process_store_fail_instruction_data_too_large() {
        solana_logger::setup();
        let keys = vec![];
        let (config_keypair, config_account) = create_config_account(keys.clone());
        let config_pubkey = config_keypair.pubkey();
        let my_config = MyConfig::new(42);

        let mut instruction = config_instruction::store(&config_pubkey, true, keys, &my_config);
        instruction.data = vec![0; 123]; // <-- Replace data with a vector that's too large
        let accounts = vec![(true, false, &config_pubkey, &config_account)];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::InvalidInstructionData)
        );
    }

    #[test]
    fn test_process_store_fail_account0_not_signer() {
        solana_logger::setup();
        let keys = vec![];
        let (config_keypair, config_account) = create_config_account(keys);
        let config_pubkey = config_keypair.pubkey();
        let my_config = MyConfig::new(42);

        let mut instruction = config_instruction::store(&config_pubkey, true, vec![], &my_config);
        instruction.accounts[0].is_signer = false; // <----- not a signer
        let accounts = vec![(false, false, &config_pubkey, &config_account)];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::MissingRequiredSignature)
        );
    }

    #[test]
    fn test_process_store_with_additional_signers() {
        solana_logger::setup();
        let pubkey = solana_sdk::pubkey::new_rand();
        let signer0_pubkey = solana_sdk::pubkey::new_rand();
        let signer1_pubkey = solana_sdk::pubkey::new_rand();
        let keys = vec![
            (pubkey, false),
            (signer0_pubkey, true),
            (signer1_pubkey, true),
        ];
        let (config_keypair, config_account) = create_config_account(keys.clone());
        let config_pubkey = config_keypair.pubkey();
        let my_config = MyConfig::new(42);

        let instruction = config_instruction::store(&config_pubkey, true, keys.clone(), &my_config);
        let signer0_account = RefCell::new(AccountSharedData::default());
        let signer1_account = RefCell::new(AccountSharedData::default());
        let accounts = vec![
            (true, false, &config_pubkey, &config_account),
            (true, false, &signer0_pubkey, &signer0_account),
            (true, false, &signer1_pubkey, &signer1_account),
        ];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(())
        );
        let meta_data: ConfigKeys = deserialize(&config_account.borrow().data()).unwrap();
        assert_eq!(meta_data.keys, keys);
        assert_eq!(
            Some(my_config),
            deserialize(get_config_data(&config_account.borrow().data()).unwrap()).ok()
        );
    }

    #[test]
    fn test_process_store_without_config_signer() {
        solana_logger::setup();
        let pubkey = solana_sdk::pubkey::new_rand();
        let signer0_pubkey = solana_sdk::pubkey::new_rand();
        let keys = vec![(pubkey, false), (signer0_pubkey, true)];
        let (config_keypair, _) = create_config_account(keys.clone());
        let config_pubkey = config_keypair.pubkey();
        let my_config = MyConfig::new(42);

        let instruction = config_instruction::store(&config_pubkey, false, keys, &my_config);
        let signer0_account = RefCell::new(AccountSharedData::from(Account {
            owner: id(),
            ..Account::default()
        }));
        let accounts = vec![(true, false, &signer0_pubkey, &signer0_account)];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::InvalidAccountData)
        );
    }

    #[test]
    fn test_process_store_with_bad_additional_signer() {
        solana_logger::setup();
        let signer0_pubkey = solana_sdk::pubkey::new_rand();
        let signer1_pubkey = solana_sdk::pubkey::new_rand();
        let signer0_account = RefCell::new(AccountSharedData::default());
        let signer1_account = RefCell::new(AccountSharedData::default());
        let keys = vec![(signer0_pubkey, true)];
        let (config_keypair, config_account) = create_config_account(keys.clone());
        let config_pubkey = config_keypair.pubkey();
        let my_config = MyConfig::new(42);

        let instruction = config_instruction::store(&config_pubkey, true, keys, &my_config);

        // Config-data pubkey doesn't match signer
        let accounts = vec![
            (true, false, &config_pubkey, &config_account),
            (true, false, &signer1_pubkey, &signer1_account),
        ];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::MissingRequiredSignature)
        );

        // Config-data pubkey not a signer
        let accounts = vec![
            (true, false, &config_pubkey, &config_account),
            (false, false, &signer0_pubkey, &signer0_account),
        ];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::MissingRequiredSignature)
        );
    }

    #[test]
    fn test_config_updates() {
        solana_logger::setup();
        let pubkey = solana_sdk::pubkey::new_rand();
        let signer0_pubkey = solana_sdk::pubkey::new_rand();
        let signer1_pubkey = solana_sdk::pubkey::new_rand();
        let signer2_pubkey = solana_sdk::pubkey::new_rand();
        let signer0_account = RefCell::new(AccountSharedData::default());
        let signer1_account = RefCell::new(AccountSharedData::default());
        let signer2_account = RefCell::new(AccountSharedData::default());
        let keys = vec![
            (pubkey, false),
            (signer0_pubkey, true),
            (signer1_pubkey, true),
        ];
        let (config_keypair, config_account) = create_config_account(keys.clone());
        let config_pubkey = config_keypair.pubkey();
        let my_config = MyConfig::new(42);

        let instruction = config_instruction::store(&config_pubkey, true, keys.clone(), &my_config);
        let accounts = vec![
            (true, false, &config_pubkey, &config_account),
            (true, false, &signer0_pubkey, &signer0_account),
            (true, false, &signer1_pubkey, &signer1_account),
        ];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(())
        );

        // Update with expected signatures
        let new_config = MyConfig::new(84);
        let instruction =
            config_instruction::store(&config_pubkey, false, keys.clone(), &new_config);
        let accounts = vec![
            (false, false, &config_pubkey, &config_account),
            (true, false, &signer0_pubkey, &signer0_account),
            (true, false, &signer1_pubkey, &signer1_account),
        ];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(())
        );
        let meta_data: ConfigKeys = deserialize(&config_account.borrow().data()).unwrap();
        assert_eq!(meta_data.keys, keys);
        assert_eq!(
            new_config,
            MyConfig::deserialize(get_config_data(&config_account.borrow().data()).unwrap())
                .unwrap()
        );

        // Attempt update with incomplete signatures
        let keys = vec![(pubkey, false), (signer0_pubkey, true)];
        let instruction = config_instruction::store(&config_pubkey, false, keys, &my_config);
        let accounts = vec![
            (false, false, &config_pubkey, &config_account),
            (true, false, &signer0_pubkey, &signer0_account),
            (false, false, &signer1_pubkey, &signer1_account),
        ];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::MissingRequiredSignature)
        );

        // Attempt update with incorrect signatures
        let keys = vec![
            (pubkey, false),
            (signer0_pubkey, true),
            (signer2_pubkey, true),
        ];
        let instruction = config_instruction::store(&config_pubkey, false, keys, &my_config);
        let accounts = vec![
            (false, false, &config_pubkey, &config_account),
            (true, false, &signer0_pubkey, &signer0_account),
            (true, false, &signer2_pubkey, &signer2_account),
        ];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::MissingRequiredSignature)
        );
    }

    #[test]
    fn test_config_updates_requiring_config() {
        solana_logger::setup();
        let pubkey = solana_sdk::pubkey::new_rand();
        let signer0_pubkey = solana_sdk::pubkey::new_rand();
        let signer0_account = RefCell::new(AccountSharedData::default());
        let keys = vec![
            (pubkey, false),
            (signer0_pubkey, true),
            (signer0_pubkey, true),
        ]; // Dummy keys for account sizing
        let (config_keypair, config_account) = create_config_account(keys);
        let config_pubkey = config_keypair.pubkey();
        let my_config = MyConfig::new(42);

        let keys = vec![
            (pubkey, false),
            (signer0_pubkey, true),
            (config_keypair.pubkey(), true),
        ];

        let instruction = config_instruction::store(&config_pubkey, true, keys.clone(), &my_config);
        let accounts = vec![
            (true, false, &config_pubkey, &config_account),
            (true, false, &signer0_pubkey, &signer0_account),
        ];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(())
        );

        // Update with expected signatures
        let new_config = MyConfig::new(84);
        let instruction =
            config_instruction::store(&config_pubkey, true, keys.clone(), &new_config);
        let accounts = vec![
            (true, false, &config_pubkey, &config_account),
            (true, false, &signer0_pubkey, &signer0_account),
        ];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Ok(())
        );
        let meta_data: ConfigKeys = deserialize(&config_account.borrow().data()).unwrap();
        assert_eq!(meta_data.keys, keys);
        assert_eq!(
            new_config,
            MyConfig::deserialize(get_config_data(&config_account.borrow().data()).unwrap())
                .unwrap()
        );

        // Attempt update with incomplete signatures
        let keys = vec![(pubkey, false), (config_keypair.pubkey(), true)];
        let instruction = config_instruction::store(&config_pubkey, true, keys, &my_config);
        let accounts = vec![(true, false, &config_pubkey, &config_account)];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::MissingRequiredSignature)
        );
    }

    #[test]
    fn test_config_initialize_no_panic() {
        let from_pubkey = solana_sdk::pubkey::new_rand();
        let config_pubkey = solana_sdk::pubkey::new_rand();
        let instructions =
            config_instruction::create_account::<MyConfig>(&from_pubkey, &config_pubkey, 1, vec![]);
        let accounts = vec![];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instructions[1].data,
                &mut MockInvokeContext::new(keyed_accounts)
            ),
            Err(InstructionError::NotEnoughAccountKeys)
        );
    }

    #[test]
    fn test_config_bad_owner() {
        let from_pubkey = solana_sdk::pubkey::new_rand();
        let config_pubkey = solana_sdk::pubkey::new_rand();
        let new_config = MyConfig::new(84);
        let signer0_pubkey = solana_sdk::pubkey::new_rand();
        let signer0_account = RefCell::new(AccountSharedData::default());
        let config_account = RefCell::new(AccountSharedData::default());
        let keys = vec![
            (from_pubkey, false),
            (signer0_pubkey, true),
            (config_pubkey, true),
        ];

        let instruction = config_instruction::store(&config_pubkey, true, keys, &new_config);
        let accounts = vec![
            (true, false, &config_pubkey, &config_account),
            (true, false, &signer0_pubkey, &signer0_account),
        ];
        let keyed_accounts = create_keyed_accounts_unified(&accounts);
        assert_eq!(
            process_instruction(
                &id(),
                &instruction.data,
                &mut MockInvokeContext::new(keyed_accounts),
            ),
            Err(InstructionError::InvalidAccountOwner)
        );
    }
}
