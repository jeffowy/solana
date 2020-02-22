//! A command-line executable for generating the chain's genesis config.

use chrono::{TimeZone, Utc};
use clap::{crate_description, crate_name, value_t, value_t_or_exit, App, Arg, ArgMatches};
use solana_clap_utils::{
    input_parsers::{pubkey_of, unix_timestamp_from_rfc3339_datetime},
    input_validators::{is_rfc3339_datetime, is_valid_percentage},
};
use solana_genesis::{genesis_accounts::add_genesis_accounts, Base64Account};
use solana_ledger::{
    blockstore::create_new_ledger, poh::compute_hashes_per_tick,
    shred_version::compute_shred_version,
};
use solana_sdk::{
    account::Account,
    clock,
    epoch_schedule::EpochSchedule,
    fee_calculator::FeeCalculator,
    genesis_config::{GenesisConfig, OperatingMode},
    native_token::lamports_to_sol,
    native_token::sol_to_lamports,
    poh_config::PohConfig,
    pubkey::Pubkey,
    rent::Rent,
    signature::{Keypair, Signer},
    system_program, timing,
};
use solana_stake_program::stake_state::{self, StakeState};
use solana_storage_program::storage_contract;
use solana_vote_program::vote_state::{self, VoteState};
use std::{
    collections::{BTreeMap, HashMap},
    error,
    fs::File,
    io,
    path::PathBuf,
    str::FromStr,
    time::Duration,
};

pub enum AccountFileFormat {
    Pubkey,
    Keypair,
}

fn required_pubkey(matches: &ArgMatches<'_>, name: &str) -> Result<Pubkey, Box<dyn error::Error>> {
    pubkey_of(matches, name).ok_or_else(|| {
        format!(
            "Invalid pubkey or file: {}",
            matches.value_of(name).unwrap()
        )
        .into()
    })
}

fn pubkey_from_str(key_str: &str) -> Result<Pubkey, Box<dyn error::Error>> {
    Pubkey::from_str(key_str).or_else(|_| {
        let bytes: Vec<u8> = serde_json::from_str(key_str)?;
        let keypair = Keypair::from_bytes(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
        Ok(keypair.pubkey())
    })
}

pub fn load_genesis_accounts(file: &str, genesis_config: &mut GenesisConfig) -> io::Result<u64> {
    let mut lamports = 0;
    let accounts_file = File::open(file.to_string())?;

    let genesis_accounts: HashMap<String, Base64Account> =
        serde_yaml::from_reader(accounts_file)
            .map_err(|err| io::Error::new(io::ErrorKind::Other, format!("{:?}", err)))?;

    for (key, account_details) in genesis_accounts {
        let pubkey = pubkey_from_str(key.as_str()).map_err(|err| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Invalid pubkey/keypair {}: {:?}", key, err),
            )
        })?;

        let owner_program_id = Pubkey::from_str(account_details.owner.as_str()).map_err(|err| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Invalid owner: {}: {:?}", account_details.owner, err),
            )
        })?;

        let mut account = Account::new(account_details.balance, 0, &owner_program_id);
        if account_details.data != "~" {
            account.data = base64::decode(account_details.data.as_str()).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Invalid account data: {}: {:?}", account_details.data, err),
                )
            })?;
        }
        account.executable = account_details.executable;
        lamports += account.lamports;
        genesis_config.add_account(pubkey, account);
    }

    Ok(lamports)
}

#[allow(clippy::cognitive_complexity)]
fn main() -> Result<(), Box<dyn error::Error>> {
    let fee_calculator = FeeCalculator::default();
    let (
        default_target_lamports_per_signature,
        default_target_signatures_per_slot,
        default_fee_burn_percentage,
    ) = {
        (
            &fee_calculator.target_lamports_per_signature.to_string(),
            &fee_calculator.target_signatures_per_slot.to_string(),
            &fee_calculator.burn_percent.to_string(),
        )
    };

    let rent = Rent::default();
    let (
        default_lamports_per_byte_year,
        default_rent_exemption_threshold,
        default_rent_burn_percentage,
    ) = {
        (
            &rent.lamports_per_byte_year.to_string(),
            &rent.exemption_threshold.to_string(),
            &rent.burn_percent.to_string(),
        )
    };

    // vote account
    let default_bootstrap_validator_lamports = &sol_to_lamports(500.0)
        .max(VoteState::get_rent_exempt_reserve(&rent))
        .to_string();
    // stake account
    let default_bootstrap_validator_stake_lamports = &sol_to_lamports(0.5)
        .max(StakeState::get_rent_exempt_reserve(&rent))
        .to_string();

    let default_target_tick_duration =
        timing::duration_as_us(&PohConfig::default().target_tick_duration);
    let default_ticks_per_slot = &clock::DEFAULT_TICKS_PER_SLOT.to_string();
    let default_operating_mode = "stable";

    let matches = App::new(crate_name!())
        .about(crate_description!())
        .version(solana_clap_utils::version!())
        .arg(
            Arg::with_name("creation_time")
                .long("creation-time")
                .value_name("RFC3339 DATE TIME")
                .validator(is_rfc3339_datetime)
                .takes_value(true)
                .help("Time when the bootstrap validator will start the cluster [default: current system time]"),
        )
        .arg(
            Arg::with_name("bootstrap_validator_pubkey_file")
                .short("b")
                .long("bootstrap-validator-pubkey")
                .value_name("BOOTSTRAP VALIDATOR PUBKEY")
                .takes_value(true)
                .required(true)
                .help("Path to file containing the bootstrap validator's pubkey"),
        )
        .arg(
            Arg::with_name("ledger_path")
                .short("l")
                .long("ledger")
                .value_name("DIR")
                .takes_value(true)
                .required(true)
                .help("Use directory as persistent ledger location"),
        )
        .arg(
            Arg::with_name("faucet_lamports")
                .short("t")
                .long("faucet-lamports")
                .value_name("LAMPORTS")
                .takes_value(true)
                .requires("faucet_pubkey_file")
                .help("Number of lamports to assign to the faucet"),
        )
        .arg(
            Arg::with_name("faucet_pubkey_file")
                .short("m")
                .long("faucet-pubkey")
                .value_name("PUBKEY")
                .takes_value(true)
                .requires("faucet_lamports")
                .help("Path to file containing the faucet's pubkey"),
        )
        .arg(
            Arg::with_name("bootstrap_vote_pubkey_file")
                .long("bootstrap-vote-pubkey")
                .value_name("BOOTSTRAP VOTE PUBKEY")
                .takes_value(true)
                .required(true)
                .help("Path to file containing the bootstrap validator's voting pubkey"),
        )
        .arg(
            Arg::with_name("bootstrap_stake_pubkey_file")
                .long("bootstrap-stake-pubkey")
                .value_name("BOOTSTRAP STAKE PUBKEY")
                .takes_value(true)
                .required(true)
                .help("Path to file containing the bootstrap validator's staking pubkey"),
        )
        .arg(
            Arg::with_name("bootstrap_stake_authorized_pubkey_file")
                .long("bootstrap-stake-authorized-pubkey")
                .value_name("BOOTSTRAP STAKE AUTHORIZED PUBKEY")
                .takes_value(true)
                .help(
                    "Path to file containing the pubkey authorized to manage the bootstrap \
                     validator's stake [default: --bootstrap-validator-pubkey]",
                ),
        )
        .arg(
            Arg::with_name("bootstrap_storage_pubkey_file")
                .long("bootstrap-storage-pubkey")
                .value_name("BOOTSTRAP STORAGE PUBKEY")
                .takes_value(true)
                .help("Path to file containing the bootstrap validator's storage pubkey"),
        )
        .arg(
            Arg::with_name("bootstrap_validator_lamports")
                .long("bootstrap-validator-lamports")
                .value_name("LAMPORTS")
                .takes_value(true)
                .default_value(default_bootstrap_validator_lamports)
                .required(true)
                .help("Number of lamports to assign to the bootstrap validator"),
        )
        .arg(
            Arg::with_name("bootstrap_validator_stake_lamports")
                .long("bootstrap-validator-stake-lamports")
                .value_name("LAMPORTS")
                .takes_value(true)
                .default_value(default_bootstrap_validator_stake_lamports)
                .required(true)
                .help("Number of lamports to assign to the bootstrap validator's stake account"),
        )
        .arg(
            Arg::with_name("target_lamports_per_signature")
                .long("target-lamports-per-signature")
                .value_name("LAMPORTS")
                .takes_value(true)
                .default_value(default_target_lamports_per_signature)
                .help(
                    "The cost in lamports that the cluster will charge for signature \
                     verification when the cluster is operating at target-signatures-per-slot",
                ),
        )
        .arg(
            Arg::with_name("lamports_per_byte_year")
                .long("lamports-per-byte-year")
                .value_name("LAMPORTS")
                .takes_value(true)
                .default_value(default_lamports_per_byte_year)
                .help(
                    "The cost in lamports that the cluster will charge per byte per year \
                     for accounts with data",
                ),
        )
        .arg(
            Arg::with_name("rent_exemption_threshold")
                .long("rent-exemption-threshold")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value(default_rent_exemption_threshold)
                .help(
                    "amount of time (in years) the balance has to include rent for \
                     to qualify as rent exempted account",
                ),
        )
        .arg(
            Arg::with_name("rent_burn_percentage")
                .long("rent-burn-percentage")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value(default_rent_burn_percentage)
                .help("percentage of collected rent to burn")
                .validator(is_valid_percentage),
        )
        .arg(
            Arg::with_name("fee_burn_percentage")
                .long("fee-burn-percentage")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value(default_fee_burn_percentage)
                .help("percentage of collected fee to burn")
                .validator(is_valid_percentage),
        )
        .arg(
            Arg::with_name("target_signatures_per_slot")
                .long("target-signatures-per-slot")
                .value_name("NUMBER")
                .takes_value(true)
                .default_value(default_target_signatures_per_slot)
                .help(
                    "Used to estimate the desired processing capacity of the cluster. \
                    When the latest slot processes fewer/greater signatures than this \
                    value, the lamports-per-signature fee will decrease/increase for \
                    the next slot. A value of 0 disables signature-based fee adjustments",
                ),
        )
        .arg(
            Arg::with_name("target_tick_duration")
                .long("target-tick-duration")
                .value_name("MILLIS")
                .takes_value(true)
                .help("The target tick rate of the cluster in milliseconds"),
        )
        .arg(
            Arg::with_name("hashes_per_tick")
                .long("hashes-per-tick")
                .value_name("NUM_HASHES|\"auto\"|\"sleep\"")
                .takes_value(true)
                .default_value("auto")
                .help(
                    "How many PoH hashes to roll before emitting the next tick. \
                     If \"auto\", determine based on --target-tick-duration \
                     and the hash rate of this computer. If \"sleep\", for development \
                     sleep for --target-tick-duration instead of hashing",
                ),
        )
        .arg(
            Arg::with_name("ticks_per_slot")
                .long("ticks-per-slot")
                .value_name("TICKS")
                .takes_value(true)
                .default_value(default_ticks_per_slot)
                .help("The number of ticks in a slot"),
        )
        .arg(
            Arg::with_name("slots_per_epoch")
                .long("slots-per-epoch")
                .value_name("SLOTS")
                .takes_value(true)
                .help("The number of slots in an epoch"),
        )
        .arg(
            Arg::with_name("enable_warmup_epochs")
                .long("enable-warmup-epochs")
                .help(
                    "When enabled epochs start short and will grow. \
                     Useful for warming up stake quickly during development"
                ),
        )
        .arg(
            Arg::with_name("primordial_accounts_file")
                .long("primordial-accounts-file")
                .value_name("FILENAME")
                .takes_value(true)
                .multiple(true)
                .help("The location of pubkey for primordial accounts and balance"),
        )
        .arg(
            Arg::with_name("operating_mode")
                .long("operating-mode")
                .possible_value("development")
                .possible_value("preview")
                .possible_value("stable")
                .takes_value(true)
                .default_value(default_operating_mode)
                .help(
                    "Selects the features that will be enabled for the cluster"
                ),
        )
        .get_matches();

    let faucet_lamports = value_t!(matches, "faucet_lamports", u64).unwrap_or(0);
    let ledger_path = PathBuf::from(matches.value_of("ledger_path").unwrap());

    let rent = Rent {
        lamports_per_byte_year: value_t_or_exit!(matches, "lamports_per_byte_year", u64),
        exemption_threshold: value_t_or_exit!(matches, "rent_exemption_threshold", f64),
        burn_percent: value_t_or_exit!(matches, "rent_burn_percentage", u8),
    };

    fn rent_exempt_check(matches: &ArgMatches<'_>, name: &str, exempt: u64) -> io::Result<u64> {
        let lamports = value_t_or_exit!(matches, name, u64);

        if lamports < exempt {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "error: insufficient {}: {} for rent exemption, requires {}",
                    name, lamports, exempt
                ),
            ))
        } else {
            Ok(lamports)
        }
    }

    let bootstrap_validator_lamports =
        value_t_or_exit!(matches, "bootstrap_validator_lamports", u64);

    let bootstrap_validator_stake_lamports = rent_exempt_check(
        &matches,
        "bootstrap_validator_stake_lamports",
        StakeState::get_rent_exempt_reserve(&rent),
    )?;

    let bootstrap_validator_pubkey = required_pubkey(&matches, "bootstrap_validator_pubkey_file")?;
    let bootstrap_vote_pubkey = required_pubkey(&matches, "bootstrap_vote_pubkey_file")?;
    let bootstrap_stake_pubkey = required_pubkey(&matches, "bootstrap_stake_pubkey_file")?;
    let bootstrap_stake_authorized_pubkey =
        pubkey_of(&matches, "bootstrap_stake_authorized_pubkey_file");
    let bootstrap_storage_pubkey = pubkey_of(&matches, "bootstrap_storage_pubkey_file");
    let faucet_pubkey = pubkey_of(&matches, "faucet_pubkey_file");

    let bootstrap_validator_vote_account = vote_state::create_account(
        &bootstrap_vote_pubkey,
        &bootstrap_validator_pubkey,
        100,
        VoteState::get_rent_exempt_reserve(&rent).max(1),
    );

    let bootstrap_validator_stake_account = stake_state::create_account(
        bootstrap_stake_authorized_pubkey
            .as_ref()
            .unwrap_or(&bootstrap_validator_pubkey),
        &bootstrap_vote_pubkey,
        &bootstrap_validator_vote_account,
        &rent,
        bootstrap_validator_stake_lamports,
    );

    let mut accounts: BTreeMap<Pubkey, Account> = [
        // node needs an account to issue votes from
        (
            bootstrap_validator_pubkey,
            Account::new(bootstrap_validator_lamports, 0, &system_program::id()),
        ),
        // where votes go to
        (bootstrap_vote_pubkey, bootstrap_validator_vote_account),
        // bootstrap validator stake
        (bootstrap_stake_pubkey, bootstrap_validator_stake_account),
    ]
    .iter()
    .cloned()
    .collect();

    if let Some(bootstrap_storage_pubkey) = bootstrap_storage_pubkey {
        accounts.insert(
            bootstrap_storage_pubkey,
            storage_contract::create_validator_storage_account(bootstrap_validator_pubkey, 1),
        );
    }

    let ticks_per_slot = value_t_or_exit!(matches, "ticks_per_slot", u64);

    let mut fee_calculator = FeeCalculator::new(
        value_t_or_exit!(matches, "target_lamports_per_signature", u64),
        value_t_or_exit!(matches, "target_signatures_per_slot", usize),
    );
    fee_calculator.burn_percent = value_t_or_exit!(matches, "fee_burn_percentage", u8);

    let mut poh_config = PohConfig::default();
    poh_config.target_tick_duration = if matches.is_present("target_tick_duration") {
        Duration::from_micros(value_t_or_exit!(matches, "target_tick_duration", u64))
    } else {
        Duration::from_micros(default_target_tick_duration)
    };

    let operating_mode = match matches.value_of("operating_mode").unwrap() {
        "development" => OperatingMode::Development,
        "stable" => OperatingMode::Stable,
        "preview" => OperatingMode::Preview,
        _ => unreachable!(),
    };

    match matches.value_of("hashes_per_tick").unwrap() {
        "auto" => match operating_mode {
            OperatingMode::Development => {
                let hashes_per_tick =
                    compute_hashes_per_tick(poh_config.target_tick_duration, 1_000_000);
                poh_config.hashes_per_tick = Some(hashes_per_tick);
            }
            OperatingMode::Stable | OperatingMode::Preview => {
                poh_config.hashes_per_tick =
                    Some(clock::DEFAULT_HASHES_PER_SECOND / clock::DEFAULT_TICKS_PER_SECOND);
            }
        },
        "sleep" => {
            poh_config.hashes_per_tick = None;
        }
        _ => {
            poh_config.hashes_per_tick = Some(value_t_or_exit!(matches, "hashes_per_tick", u64));
        }
    }

    let slots_per_epoch = if matches.value_of("slots_per_epoch").is_some() {
        value_t_or_exit!(matches, "slots_per_epoch", u64)
    } else {
        match operating_mode {
            OperatingMode::Development => clock::DEFAULT_DEV_SLOTS_PER_EPOCH,
            OperatingMode::Stable | OperatingMode::Preview => clock::DEFAULT_SLOTS_PER_EPOCH,
        }
    };
    let epoch_schedule = EpochSchedule::custom(
        slots_per_epoch,
        slots_per_epoch,
        matches.is_present("enable_warmup_epochs"),
    );

    let native_instruction_processors =
        solana_genesis_programs::get_programs(operating_mode, 0).unwrap();
    let inflation = solana_genesis_programs::get_inflation(operating_mode, 0).unwrap();

    let mut genesis_config = GenesisConfig {
        accounts,
        native_instruction_processors,
        ticks_per_slot,
        epoch_schedule,
        inflation,
        fee_calculator,
        rent,
        poh_config,
        operating_mode,
        ..GenesisConfig::default()
    };

    if let Some(creation_time) = unix_timestamp_from_rfc3339_datetime(&matches, "creation_time") {
        genesis_config.creation_time = creation_time;
    }

    if let Some(faucet_pubkey) = faucet_pubkey {
        genesis_config.add_account(
            faucet_pubkey,
            Account::new(faucet_lamports, 0, &system_program::id()),
        );
    }

    if operating_mode == OperatingMode::Development {
        solana_storage_program::rewards_pools::add_genesis_accounts(&mut genesis_config);
    }
    solana_stake_program::add_genesis_accounts(&mut genesis_config);

    if let Some(files) = matches.values_of("primordial_accounts_file") {
        for file in files {
            load_genesis_accounts(file, &mut genesis_config)?;
        }
    }

    let issued_lamports = genesis_config
        .accounts
        .iter()
        .map(|(_key, account)| account.lamports)
        .sum::<u64>();

    add_genesis_accounts(&mut genesis_config, issued_lamports - faucet_lamports);

    create_new_ledger(&ledger_path, &genesis_config)?;

    println!(
        "\
         Creation time: {}\n\
         Operating mode: {:?}\n\
         Genesis hash: {}\n\
         Shred version: {}\n\
         Hashes per tick: {:?}\n\
         Slots per epoch: {}\n\
         Warmup epochs: {}abled\n\
         {:?}\n\
         {:?}\n\
         Capitalization: {} SOL in {} accounts\n\
         ",
        Utc.timestamp(genesis_config.creation_time, 0).to_rfc3339(),
        genesis_config.operating_mode,
        genesis_config.hash(),
        compute_shred_version(&genesis_config.hash(), None),
        genesis_config.poh_config.hashes_per_tick,
        genesis_config.epoch_schedule.slots_per_epoch,
        if genesis_config.epoch_schedule.warmup {
            "en"
        } else {
            "dis"
        },
        genesis_config.rent,
        genesis_config.fee_calculator,
        lamports_to_sol(
            genesis_config
                .accounts
                .iter()
                .map(|(pubkey, account)| {
                    if account.lamports == 0 {
                        panic!("{:?}", (pubkey, account));
                    }
                    account.lamports
                })
                .sum::<u64>()
        ),
        genesis_config.accounts.len(),
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::genesis_config::GenesisConfig;
    use solana_sdk::pubkey::Pubkey;
    use std::collections::HashMap;
    use std::fs::remove_file;
    use std::io::Write;
    use std::path::Path;
    use tempfile;

    #[test]
    fn test_append_primordial_accounts_to_genesis() {
        // Test invalid file returns error
        assert!(load_genesis_accounts("unknownfile", &mut GenesisConfig::default()).is_err());

        let mut genesis_config = GenesisConfig::default();

        let mut genesis_accounts = HashMap::new();
        genesis_accounts.insert(
            Pubkey::new_rand().to_string(),
            Base64Account {
                owner: Pubkey::new_rand().to_string(),
                balance: 2 as u64,
                executable: false,
                data: String::from("aGVsbG8="),
            },
        );
        genesis_accounts.insert(
            Pubkey::new_rand().to_string(),
            Base64Account {
                owner: Pubkey::new_rand().to_string(),
                balance: 1 as u64,
                executable: true,
                data: String::from("aGVsbG8gd29ybGQ="),
            },
        );
        genesis_accounts.insert(
            Pubkey::new_rand().to_string(),
            Base64Account {
                owner: Pubkey::new_rand().to_string(),
                balance: 3 as u64,
                executable: true,
                data: String::from("bWUgaGVsbG8gdG8gd29ybGQ="),
            },
        );

        let serialized = serde_yaml::to_string(&genesis_accounts).unwrap();
        let path = Path::new("test_append_primordial_accounts_to_genesis.yml");
        let mut file = File::create(path).unwrap();
        file.write_all(&serialized.into_bytes()).unwrap();

        load_genesis_accounts(
            "test_append_primordial_accounts_to_genesis.yml",
            &mut genesis_config,
        )
        .expect("test_append_primordial_accounts_to_genesis.yml");
        // Test valid file returns ok

        remove_file(path).unwrap();

        {
            // Test all accounts were added
            assert_eq!(genesis_config.accounts.len(), genesis_accounts.len());

            // Test account data matches
            for (pubkey_str, b64_account) in genesis_accounts.iter() {
                let pubkey = pubkey_str.parse().unwrap();
                assert_eq!(
                    b64_account.owner,
                    genesis_config.accounts[&pubkey].owner.to_string()
                );

                assert_eq!(
                    b64_account.balance,
                    genesis_config.accounts[&pubkey].lamports
                );

                assert_eq!(
                    b64_account.executable,
                    genesis_config.accounts[&pubkey].executable
                );

                assert_eq!(
                    b64_account.data,
                    base64::encode(&genesis_config.accounts[&pubkey].data)
                );
            }
        }

        // Test more accounts can be appended
        let mut genesis_accounts1 = HashMap::new();
        genesis_accounts1.insert(
            Pubkey::new_rand().to_string(),
            Base64Account {
                owner: Pubkey::new_rand().to_string(),
                balance: 6 as u64,
                executable: true,
                data: String::from("eW91IGFyZQ=="),
            },
        );
        genesis_accounts1.insert(
            Pubkey::new_rand().to_string(),
            Base64Account {
                owner: Pubkey::new_rand().to_string(),
                balance: 5 as u64,
                executable: false,
                data: String::from("bWV0YSBzdHJpbmc="),
            },
        );
        genesis_accounts1.insert(
            Pubkey::new_rand().to_string(),
            Base64Account {
                owner: Pubkey::new_rand().to_string(),
                balance: 10 as u64,
                executable: false,
                data: String::from("YmFzZTY0IHN0cmluZw=="),
            },
        );

        let serialized = serde_yaml::to_string(&genesis_accounts1).unwrap();
        let path = Path::new("test_append_primordial_accounts_to_genesis.yml");
        let mut file = File::create(path).unwrap();
        file.write_all(&serialized.into_bytes()).unwrap();

        load_genesis_accounts(
            "test_append_primordial_accounts_to_genesis.yml",
            &mut genesis_config,
        )
        .expect("test_append_primordial_accounts_to_genesis.yml");

        remove_file(path).unwrap();

        // Test total number of accounts is correct
        assert_eq!(
            genesis_config.accounts.len(),
            genesis_accounts.len() + genesis_accounts1.len()
        );

        // Test old accounts are still there
        for (pubkey_str, b64_account) in genesis_accounts.iter() {
            let pubkey = &pubkey_str.parse().unwrap();
            assert_eq!(
                b64_account.balance,
                genesis_config.accounts[&pubkey].lamports,
            );
        }

        // Test new account data matches
        for (pubkey_str, b64_account) in genesis_accounts1.iter() {
            let pubkey = pubkey_str.parse().unwrap();
            assert_eq!(
                b64_account.owner,
                genesis_config.accounts[&pubkey].owner.to_string()
            );

            assert_eq!(
                b64_account.balance,
                genesis_config.accounts[&pubkey].lamports,
            );

            assert_eq!(
                b64_account.executable,
                genesis_config.accounts[&pubkey].executable,
            );

            assert_eq!(
                b64_account.data,
                base64::encode(&genesis_config.accounts[&pubkey].data),
            );
        }

        // Test accounts from keypairs can be appended
        let account_keypairs: Vec<_> = (0..3).map(|_| Keypair::new()).collect();
        let mut genesis_accounts2 = HashMap::new();
        genesis_accounts2.insert(
            serde_json::to_string(&account_keypairs[0].to_bytes().to_vec()).unwrap(),
            Base64Account {
                owner: Pubkey::new_rand().to_string(),
                balance: 20 as u64,
                executable: true,
                data: String::from("Y2F0IGRvZw=="),
            },
        );
        genesis_accounts2.insert(
            serde_json::to_string(&account_keypairs[1].to_bytes().to_vec()).unwrap(),
            Base64Account {
                owner: Pubkey::new_rand().to_string(),
                balance: 15 as u64,
                executable: false,
                data: String::from("bW9ua2V5IGVsZXBoYW50"),
            },
        );
        genesis_accounts2.insert(
            serde_json::to_string(&account_keypairs[2].to_bytes().to_vec()).unwrap(),
            Base64Account {
                owner: Pubkey::new_rand().to_string(),
                balance: 30 as u64,
                executable: true,
                data: String::from("Y29tYSBtb2Nh"),
            },
        );

        let serialized = serde_yaml::to_string(&genesis_accounts2).unwrap();
        let path = Path::new("test_append_primordial_accounts_to_genesis.yml");
        let mut file = File::create(path).unwrap();
        file.write_all(&serialized.into_bytes()).unwrap();

        load_genesis_accounts(
            "test_append_primordial_accounts_to_genesis.yml",
            &mut genesis_config,
        )
        .expect("genesis");

        solana_storage_program::rewards_pools::add_genesis_accounts(&mut genesis_config);

        remove_file(path).unwrap();

        // Test total number of accounts is correct
        assert_eq!(
            genesis_config.accounts.len(),
            genesis_accounts.len() + genesis_accounts1.len() + genesis_accounts2.len()
        );

        // Test old accounts are still there
        for (pubkey_str, b64_account) in genesis_accounts {
            let pubkey = pubkey_str.parse().unwrap();
            assert_eq!(
                b64_account.balance,
                genesis_config.accounts[&pubkey].lamports,
            );
        }

        // Test new account data matches
        for (pubkey_str, b64_account) in genesis_accounts1 {
            let pubkey = pubkey_str.parse().unwrap();
            assert_eq!(
                b64_account.owner,
                genesis_config.accounts[&pubkey].owner.to_string(),
            );

            assert_eq!(
                b64_account.balance,
                genesis_config.accounts[&pubkey].lamports,
            );

            assert_eq!(
                b64_account.executable,
                genesis_config.accounts[&pubkey].executable,
            );

            assert_eq!(
                b64_account.data,
                base64::encode(&genesis_config.accounts[&pubkey].data),
            );
        }

        // Test account data for keypairs matches
        account_keypairs.iter().for_each(|keypair| {
            let keypair_str = serde_json::to_string(&keypair.to_bytes().to_vec()).unwrap();
            let pubkey = keypair.pubkey();
            assert_eq!(
                genesis_accounts2[&keypair_str].owner,
                genesis_config.accounts[&pubkey].owner.to_string(),
            );

            assert_eq!(
                genesis_accounts2[&keypair_str].balance,
                genesis_config.accounts[&pubkey].lamports,
            );

            assert_eq!(
                genesis_accounts2[&keypair_str].executable,
                genesis_config.accounts[&pubkey].executable,
            );

            assert_eq!(
                genesis_accounts2[&keypair_str].data,
                base64::encode(&genesis_config.accounts[&pubkey].data),
            );
        });
    }

    #[test]
    fn test_genesis_account_struct_compatibility() {
        let yaml_string_pubkey = "---
98frSc8R8toHoS3tQ1xWSvHCvGEADRM9hAm5qmUKjSDX:
  balance: 4
  owner: Gw6S9CPzR8jHku1QQMdiqcmUKjC2dhJ3gzagWduA6PGw
  data:
  executable: true
88frSc8R8toHoS3tQ1xWSvHCvGEADRM9hAm5qmUKjSDX:
  balance: 3
  owner: Gw7S9CPzR8jHku1QQMdiqcmUKjC2dhJ3gzagWduA6PGw
  data: ~
  executable: true
6s36rsNPDfRSvzwek7Ly3mQu9jUMwgqBhjePZMV6Acp4:
  balance: 2
  owner: DBC5d45LUHTCrq42ZmCdzc8A8ufwTaiYsL9pZY7KU6TR
  data: aGVsbG8=
  executable: false
8Y98svZv5sPHhQiPqZvqA5Z5djQ8hieodscvb61RskMJ:
  balance: 1
  owner: DSknYr8cPucRbx2VyssZ7Yx3iiRqNGD38VqVahkUvgV1
  data: aGVsbG8gd29ybGQ=
  executable: true";

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let path = tmpfile.path();
        let mut file = File::create(path).unwrap();
        file.write_all(yaml_string_pubkey.as_bytes()).unwrap();

        let mut genesis_config = GenesisConfig::default();
        load_genesis_accounts(path.to_str().unwrap(), &mut genesis_config).expect("genesis");
        remove_file(path).unwrap();

        assert_eq!(genesis_config.accounts.len(), 4);

        let yaml_string_keypair = "---
\"[17,12,234,59,35,246,168,6,64,36,169,164,219,96,253,79,238,202,164,160,195,89,9,96,179,117,255,239,32,64,124,66,233,130,19,107,172,54,86,32,119,148,4,39,199,40,122,230,249,47,150,168,163,159,83,233,97,18,25,238,103,25,253,108]\":
  balance: 20
  owner: 9ZfsP6Um1KU8d5gNzTsEbSJxanKYp5EPF36qUu4FJqgp
  data: Y2F0IGRvZw==
  executable: true
\"[36,246,244,43,37,214,110,50,134,148,148,8,205,82,233,67,223,245,122,5,149,232,213,125,244,182,26,29,56,224,70,45,42,163,71,62,222,33,229,54,73,136,53,174,128,103,247,235,222,27,219,129,180,77,225,174,220,74,201,123,97,155,159,234]\":
  balance: 15
  owner: F9dmtjJPi8vfLu1EJN4KkyoGdXGmVfSAhxz35Qo9RDCJ
  data: bW9ua2V5IGVsZXBoYW50
  executable: false
\"[103,27,132,107,42,149,72,113,24,138,225,109,209,31,158,6,26,11,8,76,24,128,131,215,156,80,251,114,103,220,111,235,56,22,87,5,209,56,53,12,224,170,10,66,82,42,11,138,51,76,120,27,166,200,237,16,200,31,23,5,57,22,131,221]\":
  balance: 30
  owner: AwAR5mAbNPbvQ4CvMeBxwWE8caigQoMC2chkWAbh2b9V
  data: Y29tYSBtb2Nh
  executable: true";

        let tmpfile = tempfile::NamedTempFile::new().unwrap();
        let path = tmpfile.path();
        let mut file = File::create(path).unwrap();
        file.write_all(yaml_string_keypair.as_bytes()).unwrap();

        let mut genesis_config = GenesisConfig::default();
        load_genesis_accounts(path.to_str().unwrap(), &mut genesis_config).expect("genesis");
        remove_file(path).unwrap();

        assert_eq!(genesis_config.accounts.len(), 3);
    }
}
