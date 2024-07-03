//! Binaries for this test should be built with `fast-runtime` feature enabled:
//! `cargo build -r -F fast-runtime -p polkadot-parachain-bin && \`
//! `cargo build -r -F fast-runtime --bin polkadot --bin polkadot-execute-worker --bin polkadot-prepare-worker`
//! 
//! Running with normal runtimes is possible but would take ages. Running fast relay runtime with
//! normal parachain runtime WILL mess things up.

use tokio::time::Duration;
use rococo::api::runtime_types::{
	staging_xcm::v4::{
		asset::{Asset, AssetId, Assets, Fungibility},
		junction::Junction,
		junctions::Junctions,
		location::Location,
	},
	xcm::{VersionedAssets, VersionedLocation},
};
use serde_json::json;
use std::{fmt::Display, sync::{Arc, RwLock}};
use subxt::{
	blocks::ExtrinsicEvents, config::ExtrinsicParams, error::{DispatchError, TransactionError}, events::StaticEvent, tx::{Payload, Signer, TxStatus}, utils::AccountId32
};
use subxt::{OnlineClient, PolkadotConfig};
use subxt_signer::sr25519::dev;
use zombienet_sdk::{NetworkConfigBuilder, NetworkConfigExt, RegistrationStrategy};

mod coretime_rococo;
mod rococo;

use coretime_rococo::api::runtime_types::pallet_broker::types::{
	ConfigRecord as BrokerConfigRecord, Finality as BrokerFinality,
};
use coretime_rococo::api::{
	self as coretime_api, runtime_types::sp_arithmetic::per_things::Perbill,
};
use coretime_rococo::api::broker::events as broker_events;
use rococo::{api as rococo_api, api::runtime_types::polkadot_parachain_primitives::primitives};

type CoretimeRuntimeCall = coretime_api::runtime_types::coretime_rococo_runtime::RuntimeCall;
type CoretimeUtilityCall = coretime_api::runtime_types::pallet_utility::pallet::Call;
type CoretimeBrokerCall = coretime_api::runtime_types::pallet_broker::pallet::Call;

// On-demand coretime base fee (set at the genesis)
//
// NB: This fee MUST always be higher than RC's existential deposit value. Otherwise, when
//     some insta coretime is bought for the base fee value, it gets transferred from the buyer's
//     account to the pallet pot and gets burnt immediately if the pot was empty. That
//     results in everything messed up afterwards when funds have to be withdrawn from the pot and
//     teleported to the PC as revenue.
const ON_DEMAND_BASE_FEE: u128 = 50_000_000;

async fn get_total_issuance(
	relay: OnlineClient<PolkadotConfig>,
	coretime: OnlineClient<PolkadotConfig>,
) -> (u128, u128) {
	(
		relay
			.storage()
			.at_latest()
			.await
			.unwrap()
			.fetch(&rococo_api::storage().balances().total_issuance())
			.await
			.unwrap()
			.unwrap(),
		coretime
			.storage()
			.at_latest()
			.await
			.unwrap()
			.fetch(&rococo_api::storage().balances().total_issuance())
			.await
			.unwrap()
			.unwrap(),
	)
}

async fn assert_total_issuance(
	relay: OnlineClient<PolkadotConfig>,
	coretime: OnlineClient<PolkadotConfig>,
	ti: (u128, u128),
) {
	let actual_ti = get_total_issuance(relay, coretime).await;
	log::debug!("Asserting total issuance: actual: {actual_ti:?}, expected: {ti:?}");
	assert_eq!(ti, actual_ti);
}

type ParaEvents<C> = Arc<RwLock<Vec<(u64, subxt::events::EventDetails<C>)>>>;

async fn para_watcher<C: subxt::Config + Clone>(api: OnlineClient<C>, events: ParaEvents<C>)
where
	<C::Header as subxt::config::Header>::Number: Display,
{
	let mut blocks_sub = api.blocks().subscribe_finalized().await.unwrap();

	log::debug!("Starting parachain watcher");
	while let Some(block) = blocks_sub.next().await {
		let block = block.unwrap();
		log::debug!("Finalized parachain block {}", block.number());

		for event in block.events().await.unwrap().iter() {
			let event = event.unwrap();
			log::debug!("Got event: {} :: {}", event.pallet_name(), event.variant_name());
			{
				events.write().unwrap().push((block.number().into(), event.clone()));
			}
			if event.pallet_name() == "Broker" {
				match event.variant_name() {
					"Purchased" => log::trace!(
						"{:#?}",
						event
							.as_event::<broker_events::Purchased>()
							.unwrap()
							.unwrap()
					),
					"SaleInitialized" => log::trace!(
						"{:#?}",
						event
							.as_event::<broker_events::SaleInitialized>()
							.unwrap()
							.unwrap()
					),
					"HistoryInitialized" => log::trace!(
						"{:#?}",
						event
							.as_event::<broker_events::HistoryInitialized>()
							.unwrap()
							.unwrap()
					),
					"CoreAssigned" => log::trace!(
						"{:#?}",
						event
							.as_event::<broker_events::CoreAssigned>()
							.unwrap()
							.unwrap()
					),
					"Pooled" => log::trace!(
						"{:#?}",
						event.as_event::<broker_events::Pooled>().unwrap().unwrap()
					),
					"ClaimsReady" => log::trace!(
						"{:#?}",
						event
							.as_event::<broker_events::ClaimsReady>()
							.unwrap()
							.unwrap()
					),
					"RevenueClaimBegun" => log::trace!(
						"{:#?}",
						event
							.as_event::<broker_events::RevenueClaimBegun>()
							.unwrap()
							.unwrap()
					),
					"RevenueClaimItem" => log::trace!(
						"{:#?}",
						event
							.as_event::<broker_events::RevenueClaimItem>()
							.unwrap()
							.unwrap()
					),
					"RevenueClaimPaid" => log::trace!(
						"{:#?}",
						event
							.as_event::<broker_events::RevenueClaimPaid>()
							.unwrap()
							.unwrap()
					),
					_ => (),
				}
			}
		}
	}
}

async fn wait_for_para_event<C: subxt::Config + Clone, E: StaticEvent, P: Fn(&E) -> bool + Copy>(
	events: ParaEvents<C>,
	pallet: &'static str,
	variant: &'static str,
	predicate: P,
) -> E {
	loop {
		let mut events = events.write().unwrap();
		if let Some(entry) = events.iter().find(|&e| {
			e.1.pallet_name() == pallet &&
				e.1.variant_name() == variant &&
				predicate(&e.1.as_event::<E>().unwrap().unwrap())
		}) {
			let entry = entry.clone();
			events.retain(|e| e.0 > entry.0);
			return entry.1.as_event::<E>().unwrap().unwrap();
		}
		drop(events);
		tokio::time::sleep(std::time::Duration::from_secs(6)).await;
	}
}

async fn ti_watcher<C: subxt::Config + Clone>(api: OnlineClient<C>, prefix: &'static str)
where
	<C::Header as subxt::config::Header>::Number: Display,
{
	let mut blocks_sub = api.blocks().subscribe_finalized().await.unwrap();

	let mut issuance = 0i128;

	log::debug!("Starting parachain watcher");
	while let Some(block) = blocks_sub.next().await {
		let block = block.unwrap();

		let ti = api
			.storage()
			.at(block.reference())
			.fetch(&rococo_api::storage().balances().total_issuance())
			.await
			.unwrap()
			.unwrap() as i128;

		let diff = ti - issuance;
		if diff != 0 {
			log::info!("{} #{} issuance {} ({:+})", prefix, block.number(), ti, diff);
		}
		issuance = ti;
	}
}

async fn balance<C: subxt::Config>(api: OnlineClient<C>, acc: &AccountId32) -> u128 {
	api.storage().at_latest().await
	.unwrap()
	.fetch(&rococo_api::storage()
		.balances()
		.account(acc.clone())
	).await
	.unwrap()
	.unwrap()
	.free
}

async fn on_failure() {
	// Keep the network running to diagnose the problem
	loop {
		tokio::time::sleep(Duration::from_secs(3600)).await;
	}
}

async fn push_tx_hard<Cll: Payload, Cfg: subxt::Config, Sgnr: Signer<Cfg>>(
	cli: OnlineClient<Cfg>,
	call: &Cll,
	signer: &Sgnr,
) -> ExtrinsicEvents<Cfg>
where
	<Cfg::ExtrinsicParams as ExtrinsicParams<Cfg>>::Params: Default,
{	
	let tx = cli.tx().create_signed(call, signer, Default::default()).await.expect("Signed transaction created ok");
	log::trace!("Created tx {:?}", tx.encoded());
	match tx.submit_and_watch().await {
	    Ok(mut progress) => {
	    	log::trace!("Submitted: {progress:?}");

	        while let Some(status) = progress.next().await {
	            match status {
	                // Finalized! Return.
	                Ok(TxStatus::InFinalizedBlock(in_block)) => {
	                	log::trace!("Transaction is in finalized block {:?}", in_block.block_hash());
	                	let events = in_block.fetch_events().await.expect("Events fetched successfully");
				        for ev in events.iter() {
				            let ev = ev.expect("Event details");
				            if ev.pallet_name() == "System" && ev.variant_name() == "ExtrinsicFailed" {
				                let dispatch_error =
				                    DispatchError::decode_from(ev.field_bytes(), cli.metadata()).expect("Dispatch error");
				                log::error!("Extrinsic failed: {dispatch_error:?}");
				                on_failure().await;
				            }
				        }
				        return events;
	                }
	                // Error scenarios; return the error.
	                Ok(TxStatus::Error { ref message }) | Ok(TxStatus::Invalid { ref message }) | Ok(TxStatus::Dropped { ref message }) => {
	                	log::error!("Transaction ERROR {status:?}: {message}");
	                	on_failure().await;
	                },
	                Ok(s) => {
	                	let v = match s {
		                    TxStatus::Validated => "Validated",
		                    TxStatus::Broadcasted { .. } => "Broadcasted",
		                    TxStatus::NoLongerInBestBlock => "NoLongerInBestBlock",
		                    TxStatus::InBestBlock(_) => "InBestBlock",
		                    TxStatus::InFinalizedBlock(_) => "InFinalizedBlock",
		                    TxStatus::Error { .. } => "Error",
		                    TxStatus::Invalid { .. } => "Invalid",
		                    TxStatus::Dropped { .. } => "Dropped",
		                };
	                	log::trace!("Transaction status IGNORED: {v}", );
	                	continue;
	                }
	                Err(e) => {
	                	log::error!("Transaction PROGRESS ERROR: {e:?}");
	                	on_failure().await;
	                }
	            }
	        }
	    },
	    Err(e) => {
	    	log::error!("Error submitting transaction: {e}");
	    	on_failure().await;
	    },
	}

	panic!("Transaction not pushed");

	// loop {
	// 	match cli.tx().sign_and_submit_then_watch_default(call, signer).await {
	// 		Ok(txp) => match txp.wait_for_finalized_success().await {
	// 			Ok(events) => return events,
	// 			Err(e) => {
	// 				log::error!("Transaction not finalized successfuly, retrying: {e:?}");
	// 				tokio::time::sleep(Duration::from_secs(1)).await;
	// 				continue
	// 			},
	// 		},
	// 		Err(e) => {
	// 			log::error!("Retrying transaction: {e:?}");
	// 			tokio::time::sleep(Duration::from_secs(1)).await;
	// 			continue
	// 		},
	// 	}
	// }
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
	// Use tracing subscriber for nicer logs from subxt/jsonrpsee
	tracing_subscriber::fmt::try_init().unwrap();

	let network = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_genesis_overrides(
					json!({ "configuration": { "config": { "scheduler_params": { "on_demand_base_fee": ON_DEMAND_BASE_FEE }}}}),
				)
				.with_node(|node| node.with_name("alice"))
				.with_node(|node| node.with_name("bob"))
				.with_node(|node| node.with_name("charlie"))
		})
		.with_parachain(|p| {
			p.with_id(1005)
				.with_default_command("polkadot-parachain")
				.with_chain("coretime-rococo-local")
				.cumulus_based(true)
				.with_registration_strategy(RegistrationStrategy::InGenesis)
				// .with_genesis_overrides(genesis_accs)
				.with_collator(|n| n.with_name("coretime"))
		})
		.build()
		.unwrap()
		.spawn_native()
		.await?;

	let relay_node = network.get_node("alice")?;
	let para_node = network.get_node("coretime")?;

	let relay_client: OnlineClient<PolkadotConfig> =
		relay_node.wait_client().await?;
	let para_client: OnlineClient<PolkadotConfig> =
		para_node.wait_client().await?;

	// Get total issuance on both sides
	let mut total_issuance = get_total_issuance(relay_client.clone(), para_client.clone()).await;
	log::info!("Reference total issuance: {total_issuance:?}");

	// Prepare everything
	let alice = dev::alice();
	let alice_acc = AccountId32(alice.public_key().0);

	let bob = dev::bob();
	let bob_acc = AccountId32(bob.public_key().0);

	let para_events: ParaEvents<PolkadotConfig> = Arc::new(RwLock::new(Vec::new()));
	let p_api = para_node.wait_client().await?;
	let p_events = para_events.clone();

	let _subscriber = tokio::spawn(async move {
		para_watcher(p_api, p_events).await;
	});

	let api: OnlineClient<PolkadotConfig> = para_node.wait_client().await?;
	let _s1 = tokio::spawn(async move {
		ti_watcher(api, "PARA").await;
	});
	let api: OnlineClient<PolkadotConfig> = relay_node.wait_client().await?;
	let _s2 = tokio::spawn(async move {
		ti_watcher(api, "RELAY").await;
	});

	log::info!("Initiating teleport from RC's account of Alice to PC's one");

	// Teleport some Alice's tokens to the Coretime chain. Although her account is pre-funded on
	// the PC, that is still neccessary to bootstrap RC's `CheckedAccount`.
	push_tx_hard(
		relay_client.clone(),
		&rococo_api::tx().xcm_pallet().teleport_assets(
			VersionedLocation::V4(Location {
				parents: 0,
				interior: Junctions::X1([Junction::Parachain(1005)]),
			}),
			VersionedLocation::V4(Location {
				parents: 0,
				interior: Junctions::X1([Junction::AccountId32 {
					network: None,
					id: alice.public_key().0,
				}]),
			}),
			VersionedAssets::V4(Assets(vec![Asset {
				id: AssetId(Location { parents: 0, interior: Junctions::Here }),
				fun: Fungibility::Fungible(1_500_000_000),
			}])),
			0,
		),
		&alice,
	)
	.await;

	wait_for_para_event(
		para_events.clone(),
		"Balances",
		"Minted",
		|e: &coretime_api::balances::events::Minted| e.who == alice_acc,
	)
	.await;

	// RC's total issuance doen't change, but PC's one increses after the teleport

	total_issuance.1 += 1_500_000_000;
	assert_total_issuance(relay_client.clone(), para_client.clone(), total_issuance).await;

	log::info!("Initializing broker and starting sales");

	// Initialize broker and start sales

	push_tx_hard(
		para_client.clone(),
		&coretime_api::tx()
			.sudo()
			.sudo(CoretimeRuntimeCall::Utility(CoretimeUtilityCall::batch {
				calls: vec![
					CoretimeRuntimeCall::Broker(CoretimeBrokerCall::configure {
						config: BrokerConfigRecord {
							advance_notice: 5,
							interlude_length: 1,
							leadin_length: 1,
							region_length: 1,
							ideal_bulk_proportion: Perbill(100),
							limit_cores_offered: None,
							renewal_bump: Perbill(10),
							contribution_timeout: 5,
						},
					}),
					CoretimeRuntimeCall::Broker(CoretimeBrokerCall::request_core_count {
						core_count: 3,
					}),
					CoretimeRuntimeCall::Broker(CoretimeBrokerCall::set_lease {
						task: 1005,
						until: 1000,
					}),
					CoretimeRuntimeCall::Broker(CoretimeBrokerCall::start_sales {
						end_price: 45_000_000,
						extra_cores: 2,
					}),
				],
			})),
		&alice,
	)
	.await;

	log::info!("Waiting for a full-length sale to begin");

	// Skip the first sale completeley as it may be a short one. Also, `request_code_count` requires
	// two session boundaries to propagate. Given that the `fast-runtime` session is 10 blocks and
	// the timeslice is 20 blocks, we should be just in time.

	let _: coretime_api::broker::events::SaleInitialized =
		wait_for_para_event(para_events.clone(), "Broker", "SaleInitialized", |_| true).await;
	log::info!("Skipped short sale");

	let sale: coretime_api::broker::events::SaleInitialized =
		wait_for_para_event(para_events.clone(), "Broker", "SaleInitialized", |_| true).await;
	log::info!("{:?}", sale);

	// Alice buys a region
	
	log::info!("Alice is going to buy a region");

	let r = push_tx_hard(
		para_client.clone(),
		&coretime_api::tx().broker().purchase(1_000_000_000),
		&alice,
	)
	.await;

	let purchase = r.find_first::<broker_events::Purchased>()?.unwrap();

	// Somewhere below this point, the revenue from this sale will be teleported to the RC and burnt
	// on both chains. Let's account that but not assert just yet.

	total_issuance.0 -= purchase.price;
	total_issuance.1 -= purchase.price;

	// Alice pools the region

	log::info!("Alice is going to put the region into the pool");

	let r = push_tx_hard(
		para_client.clone(),
		&coretime_api::tx().broker().pool(
			purchase.region_id,
			alice_acc.clone(),
			BrokerFinality::Final,
		),
		&alice,
	)
	.await;

	let pooled = r.find_first::<broker_events::Pooled>()?.unwrap();

	// Wait until the beginning of the timeslice where the region belongs to

	log::info!("Waiting for the region to begin");

	let hist = wait_for_para_event(
		para_events.clone(),
		"Broker",
		"HistoryInitialized",
		|e: &broker_events::HistoryInitialized| e.when == pooled.region_id.begin,
	)
	.await;

	// Alice's private contribution should be there

	assert!(hist.private_pool_size > 0);

	// Bob places an order to buy insta coretime as RC

	log::info!("Bob is going to buy an on-demand core");

	let r = push_tx_hard(
		relay_client.clone(),
		&rococo_api::tx()
			.on_demand_assignment_provider()
			.place_order_allow_death(100_000_000, primitives::Id(100)),
		&bob,
	)
	.await;

	// for e in r.iter() {
	// 	let e = e?;
	// 	log::info!("RELAY EVENT {} :: {}", e.pallet_name(), e.variant_name());
	// }

	let order = r
		.find_first::<rococo_api::on_demand_assignment_provider::events::OnDemandOrderPlaced>()?
		.unwrap();

	// As there's no spot traffic, Bob will only pay base fee

	assert_eq!(order.spot_price, ON_DEMAND_BASE_FEE);

	// Somewhere below this point, revenue is generated and is teleported to the PC (that happens
	// once a timeslice so we're not ready to assert it yet, let's just account). That checks out
	// tokens from the RC and mints them on the PC.

	total_issuance.1 += ON_DEMAND_BASE_FEE;

	// As soon as the PC receives the tokens, it divides them half by half into system and private
	// contributions (we have 3 cores, one is leased to Coretime itself, one is pooled by the system,
	// and one is pooled by Alice).

	// Now we're waiting for the moment when Alice may claim her revenue

	log::info!("Waiting for Alice's revenue to be ready to claim");

	let claims_ready = wait_for_para_event(
		para_events.clone(),
		"Broker",
		"ClaimsReady",
		|e: &broker_events::ClaimsReady| e.when == pooled.region_id.begin,
	)
	.await;

	// The revenue should be half of the spot price, which is equal to the base fee.

	assert_eq!(claims_ready.private_payout, ON_DEMAND_BASE_FEE / 2);

	// By this moment, we're sure that revenue was received by the PC and can assert the total
	// issuance

	assert_total_issuance(relay_client.clone(), para_client.clone(), total_issuance).await;

	// Alice claims her revenue

	log::info!("Alice is going to claim her revenue");

	// let relay_client: OnlineClient<PolkadotConfig> =
	// 	network.get_node("alice")?.wait_client().await?;
	// let para_client: OnlineClient<PolkadotConfig> =
	// 	network.get_node("coretime")?.wait_client().await?;

	let r = push_tx_hard(
		para_client.clone(),
		&coretime_api::tx().broker().claim_revenue(pooled.region_id, pooled.duration),
		&alice,
	)
	.await;

	let claim_paid = r.find_first::<broker_events::RevenueClaimPaid>()?.unwrap();

	// let claim_paid = wait_for_para_event(
	// 	para_events.clone(),
	// 	"Broker",
	// 	"RevenueClaimPaid",
	// 	|e: &broker_events::RevenueClaimPaid| e.who == alice_acc,
	// )
	// .await;

	log::info!("Revenue claimed, waiting for 2 timeslices until the system revenue is burnt");

	assert_eq!(claim_paid.amount, ON_DEMAND_BASE_FEE / 2);

	// As for the system revenue, it is teleported back to the RC and burnt there. Those burns are
	// batched and are processed once a timeslice, after a new one starts. So we have to wait for two
	// timeslice boundaries to pass to be sure the teleport has already happened somewhere in between.

	let _: coretime_api::broker::events::SaleInitialized =
		wait_for_para_event(para_events.clone(), "Broker", "SaleInitialized", |_| true).await;

	total_issuance.0 -= ON_DEMAND_BASE_FEE / 2;
	total_issuance.1 -= ON_DEMAND_BASE_FEE / 2;

	let _: coretime_api::broker::events::SaleInitialized =
		wait_for_para_event(para_events.clone(), "Broker", "SaleInitialized", |_| true).await;

	assert_total_issuance(relay_client.clone(), para_client.clone(), total_issuance).await;

	log::info!("Test finished successfuly");

	Ok(())
}
