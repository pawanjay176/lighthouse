use crate::{checks, LocalNetwork};
use clap::ArgMatches;
use eth2_libp2p::Enr;
use futures::{future, stream, Future, Stream};
use node_test_rig::{
    environment::EnvironmentBuilder, testing_client_config, ClientGenesis, ValidatorConfig,
};
use std::io::prelude::*;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempdir::TempDir;

pub fn run_no_eth1_sim(matches: &ArgMatches) -> Result<(), String> {
    let node_count = value_t!(matches, "nodes", usize).expect("missing nodes default");
    let validators_per_node = value_t!(matches, "validators_per_node", usize)
        .expect("missing validators_per_node default");
    let speed_up_factor =
        value_t!(matches, "speed_up_factor", u64).expect("missing speed_up_factor default");
    let mut end_after_checks = true;
    if matches.is_present("end_after_checks") {
        end_after_checks = false;
    }

    println!("Beacon Chain Simulator:");
    println!(" nodes:{}", node_count);
    println!(" validators_per_node:{}", validators_per_node);
    println!(" end_after_checks:{}", end_after_checks);

    let log_level = "trace";
    let log_format = None;

    let mut env = EnvironmentBuilder::mainnet()
        .async_logger(log_level, log_format)?
        .multi_threaded_tokio_runtime()?
        .build()?;

    let eth1_block_time = Duration::from_millis(15_000 / speed_up_factor);

    let spec = &mut env.eth2_config.spec;

    spec.milliseconds_per_slot /= speed_up_factor;
    spec.eth1_follow_distance = 16;
    spec.min_genesis_delay = eth1_block_time.as_secs() * spec.eth1_follow_distance * 2;
    spec.min_genesis_time = 0;
    spec.min_genesis_active_validator_count = 64;
    spec.seconds_per_eth1_block = 1;

    let genesis_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "should get system time")?
        + Duration::from_secs(5);

    let slot_duration = Duration::from_millis(spec.milliseconds_per_slot);
    let total_validator_count = validators_per_node * node_count;

    let context = env.core_context();

    let mut beacon_config = testing_client_config();

    beacon_config.genesis = ClientGenesis::Interop {
        validator_count: total_validator_count,
        genesis_time: genesis_time.as_secs(),
    };
    beacon_config.dummy_eth1_backend = true;
    beacon_config.sync_eth1_chain = true;

    let first_data_dir = std::path::PathBuf::from("/Users/mangala/pawan/testing_stuff");
    beacon_config.data_dir = first_data_dir;
    beacon_config.network.enr_address = Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));

    let future = LocalNetwork::new1(context, beacon_config.clone(), 12345, "node1".into())
        /*
         * One by one, add beacon nodes to the network.
         */
        .and_then(move |network| {
            let network_1 = network.clone();

            // Note: presently the validator client future will only resolve once genesis time
            // occurs. This is great for this scenario, but likely to change in the future.
            //
            // If the validator client future behaviour changes, we would need to add a new future
            // that delays until genesis. Otherwise, all of the checks that start in the next
            // future will start too early.

            network_1
                .add_validator_client(ValidatorConfig::default(), 0, vec![0, 1, 2, 3])
                .map(|_| network)
        })
        .and_then(move |network| {
            // The `final_future` either completes immediately or never completes, depending on the value
            // of `end_after_checks`.
            let final_future: Box<dyn Future<Item = (), Error = String> + Send> =
                Box::new(future::empty().map_err(|()| "".to_string()));
            future::ok(())
                // Check that the chain finalizes at the first given opportunity.
                .join(checks::verify_first_finalization(
                    network.clone(),
                    slot_duration,
                ))
                // // Check that the chain starts with the expected validator count.
                // .join(checks::verify_initial_validator_count(
                //     network.clone(),
                //     slot_duration,
                //     initial_validator_count,
                // ))
                // Check that validators greater than `spec.min_genesis_active_validator_count` are
                // onboarded at the first possible opportunity.
                .join(checks::verify_validator_onboarding(
                    network.clone(),
                    slot_duration,
                    total_validator_count,
                ))
                .join(final_future)
                .map(|_| ())
        });

    // env.runtime().block_on(future)

    let mut env1 = EnvironmentBuilder::mainnet()
        .async_logger(log_level, log_format)?
        .multi_threaded_tokio_runtime()?
        .build()?;

    let spec = &mut env1.eth2_config.spec;

    spec.milliseconds_per_slot /= speed_up_factor;
    spec.eth1_follow_distance = 16;
    spec.min_genesis_delay = eth1_block_time.as_secs() * spec.eth1_follow_distance * 2;
    spec.min_genesis_time = 0;
    spec.min_genesis_active_validator_count = 64;
    spec.seconds_per_eth1_block = 1;

    let slot_duration = Duration::from_millis(spec.milliseconds_per_slot);
    let total_validator_count = validators_per_node * node_count;

    let context = env1.core_context();

    let mut beacon_config1 = testing_client_config();

    beacon_config1.genesis = ClientGenesis::Interop {
        validator_count: total_validator_count,
        genesis_time: genesis_time.as_secs(),
    };
    beacon_config1.dummy_eth1_backend = true;
    beacon_config1.sync_eth1_chain = true;

    let second_data_dir = std::path::PathBuf::from("/Users/mangala/pawan/testing_stuff1");
    beacon_config1.data_dir = second_data_dir.clone();
    beacon_config1.network.enr_address = Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));

    let enr_folder = beacon_config.data_dir.clone().join("network");
    dbg!(&enr_folder);
    let mut enr_file = std::fs::File::open(enr_folder.join("enr.dat")).unwrap();
    let mut enr_str = String::new();
    enr_file.read_to_string(&mut enr_str).unwrap();
    let enr = Enr::from_str(&enr_str).unwrap();

    dbg!(&enr);

    beacon_config1.network.boot_nodes.push(enr);

    let future1 = LocalNetwork::new1(context, beacon_config1.clone(), 12346, "node2".into())
        /*
         * One by one, add beacon nodes to the network.
         */
        .and_then(move |network| {
            let network_1 = network.clone();

            // Note: presently the validator client future will only resolve once genesis time
            // occurs. This is great for this scenario, but likely to change in the future.
            //
            // If the validator client future behaviour changes, we would need to add a new future
            // that delays until genesis. Otherwise, all of the checks that start in the next
            // future will start too early.

            network_1
                .add_validator_client(ValidatorConfig::default(), 0, vec![4, 5, 6, 7])
                .map(|_| network)
        })
        .and_then(move |network| {
            // The `final_future` either completes immediately or never completes, depending on the value
            // of `end_after_checks`.
            let final_future: Box<dyn Future<Item = (), Error = String> + Send> =
                Box::new(future::empty().map_err(|()| "".to_string()));
            future::ok(())
                // Check that the chain finalizes at the first given opportunity.
                .join(checks::verify_first_finalization(
                    network.clone(),
                    slot_duration,
                ))
                // // Check that the chain starts with the expected validator count.
                // .join(checks::verify_initial_validator_count(
                //     network.clone(),
                //     slot_duration,
                //     initial_validator_count,
                // ))
                // Check that validators greater than `spec.min_genesis_active_validator_count` are
                // onboarded at the first possible opportunity.
                .join(checks::verify_validator_onboarding(
                    network.clone(),
                    slot_duration,
                    total_validator_count,
                ))
                .join(final_future)
                .map(|_| ())
        });

    env.runtime().spawn(futures::lazy(move || {
        future.map_err(|e| println!("Error in client 1 {}", e))
    }));

    env1.runtime().spawn(futures::lazy(move || {
        future1.map_err(|e| println!("Error in client 2 {}", e))
    }));

    loop {}

    Ok(())
}
