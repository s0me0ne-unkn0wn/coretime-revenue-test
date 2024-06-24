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
use std::sync::{Arc, RwLock};
use subxt::{
	blocks::ExtrinsicEvents,
	config::ExtrinsicParams,
	events::StaticEvent,
	tx::{Payload, Signer},
	utils::AccountId32,
};
// use subxt::config::SubstrateExtrinsicParamsBuilder;
use subxt::{OnlineClient, PolkadotConfig};
use subxt_signer::sr25519::dev;
use zombienet_sdk::{NetworkConfigBuilder, NetworkConfigExt, RegistrationStrategy};

// #[subxt::subxt(runtime_metadata_path = "polkadot_metadata_full.scale")]
// #[subxt::subxt(runtime_metadata_path = "rococo-metadata.scale")]
// pub mod rococo {}

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

// const BEST_BLOCK_HEIGHT: &str = "block_height{status=\"best\"}";

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
	log::info!("Asserting total issuance: actual: {actual_ti:?}, expected: {ti:?}");
	assert_eq!(ti, actual_ti);
}

type ParaEvents<C> = Arc<RwLock<Vec<(u64, subxt::events::EventDetails<C>)>>>;

async fn para_watcher<C: subxt::Config + Clone>(api: OnlineClient<C>, events: ParaEvents<C>)
where
	<C::Header as subxt::config::Header>::Number: std::fmt::Display,
{
	let mut blocks_sub = api.blocks().subscribe_finalized().await.unwrap();

	log::debug!("Starting parachain watcher");
	while let Some(block) = blocks_sub.next().await {
		let block = block.unwrap();
		log::info!("Finalized parachain block {}", block.number());

		for event in block.events().await.unwrap().iter() {
			let event = event.unwrap();
			log::info!("Got event: {} :: {}", event.pallet_name(), event.variant_name());
			events.write().unwrap().push((block.number().into(), event.clone()));
			if event.pallet_name() == "Broker" {
				match event.variant_name() {
					"Purchased" => log::info!(
						"{:#?}",
						event
							.as_event::<coretime_api::broker::events::Purchased>()
							.unwrap()
							.unwrap()
					),
					"SaleInitialized" => log::info!(
						"{:#?}",
						event
							.as_event::<coretime_api::broker::events::SaleInitialized>()
							.unwrap()
							.unwrap()
					),
					"HistoryInitialized" => log::info!(
						"{:#?}",
						event
							.as_event::<coretime_api::broker::events::HistoryInitialized>()
							.unwrap()
							.unwrap()
					),
					"CoreAssigned" => log::info!(
						"{:#?}",
						event
							.as_event::<coretime_api::broker::events::CoreAssigned>()
							.unwrap()
							.unwrap()
					),
					"Pooled" => log::info!(
						"{:#?}",
						event.as_event::<coretime_api::broker::events::Pooled>().unwrap().unwrap()
					),
					"ClaimsReady" => log::info!(
						"{:#?}",
						event
							.as_event::<coretime_api::broker::events::ClaimsReady>()
							.unwrap()
							.unwrap()
					),
					"RevenueClaimBegun" => log::info!(
						"{:#?}",
						event
							.as_event::<coretime_api::broker::events::RevenueClaimBegun>()
							.unwrap()
							.unwrap()
					),
					"RevenueClaimItem" => log::info!(
						"{:#?}",
						event
							.as_event::<coretime_api::broker::events::RevenueClaimItem>()
							.unwrap()
							.unwrap()
					),
					"RevenueClaimPaid" => log::info!(
						"{:#?}",
						event
							.as_event::<coretime_api::broker::events::RevenueClaimPaid>()
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
	<C::Header as subxt::config::Header>::Number: std::fmt::Display,
{
	let mut blocks_sub = api.blocks().subscribe_finalized().await.unwrap();

	let mut issuance = 0i128;

	log::debug!("Starting parachain watcher");
	while let Some(block) = blocks_sub.next().await {
		let block = block.unwrap();

		let ti = api
			.storage()
			.at(block.reference())
			// .await
			// .unwrap()
			.fetch(&rococo_api::storage().balances().total_issuance())
			.await
			.unwrap()
			.unwrap() as i128;

		let diff = ti - issuance;
		log::info!("{} #{} issuance {} ({:+})", prefix, block.number(), ti, diff);
		issuance = ti;
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
	loop {
		match cli.tx().sign_and_submit_then_watch_default(call, signer).await {
			Ok(txp) => match txp.wait_for_finalized_success().await {
				Ok(events) => return events,
				Err(e) => {
					log::error!("Transaction not finalized successfuly, retrying: {e:?}");
					tokio::time::sleep(Duration::from_secs(1)).await;
					continue
				},
			},
			Err(e) => {
				log::error!("Retrying transaction: {e:?}");
				tokio::time::sleep(Duration::from_secs(1)).await;
				continue
			},
		}
	}
}

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
	env_logger::init_from_env(
		env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
	);

	let network = NetworkConfigBuilder::new()
		.with_relaychain(|r| {
			r.with_chain("rococo-local")
				.with_default_command("polkadot")
				.with_genesis_overrides(
					json!({ "configuration": { "config": { "scheduler_params": { "on_demand_base_fee": 50_000_000 }}}}),
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
		.await?
		// .unwrap()
		;

	let relay_client: OnlineClient<PolkadotConfig> =
		network.get_node("alice")?.wait_client().await?; //.client().await?;
	let para_client: OnlineClient<PolkadotConfig> =
		network.get_node("coretime")?.wait_client().await?;

	// Get total issuance on both sides
	let mut total_issuance = get_total_issuance(relay_client.clone(), para_client.clone()).await;
	log::info!("Reference total issuance: {total_issuance:?}");

	// Prepare everything
	let alice = dev::alice();
	let alice_acc = AccountId32(alice.public_key().0);

	let bob = dev::bob();
	let bob_acc = AccountId32(bob.public_key().0);

	let para_events: ParaEvents<PolkadotConfig> = Arc::new(RwLock::new(Vec::new()));
	let p_api = para_client.clone();
	let p_events = para_events.clone();

	let _subscriber = tokio::spawn(async move {
		para_watcher(p_api, p_events).await;
	});

	let api = para_client.clone();
	let _s1 = tokio::spawn(async move {
		ti_watcher(api, "PARA").await;
	});
	let api = relay_client.clone();
	let _s2 = tokio::spawn(async move {
		ti_watcher(api, "RELAY").await;
	});

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

	// Skip the first sale completeley as it may be a short one
	let _: coretime_api::broker::events::SaleInitialized =
		wait_for_para_event(para_events.clone(), "Broker", "SaleInitialized", |_| true).await;
	log::info!("Skipped short sale");

	let sale: coretime_api::broker::events::SaleInitialized =
		wait_for_para_event(para_events.clone(), "Broker", "SaleInitialized", |_| true).await;
	log::info!("{:?}", sale);

	// Buy a region
	let r = push_tx_hard(
		para_client.clone(),
		&coretime_api::tx().broker().purchase(1_000_000_000),
		&alice,
	)
	.await;

	let purchase = r.find_first::<broker_events::Purchased>()?.unwrap();

	// Pool the region
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
	let hist = wait_for_para_event(
		para_events.clone(),
		"Broker",
		"HistoryInitialized",
		|e: &broker_events::HistoryInitialized| e.when == pooled.region_id.begin,
	)
	.await;
	assert!(hist.private_pool_size > 0);

	let r = push_tx_hard(
		relay_client.clone(),
		&rococo_api::tx()
			.on_demand_assignment_provider()
			.place_order_allow_death(100_000_000, primitives::Id(100)),
		&bob,
	)
	.await;

	for e in r.iter() {
		let e = e?;
		log::info!("RELAY EVENT {} :: {}", e.pallet_name(), e.variant_name());
	}

	let order = r
		.find_first::<rococo_api::on_demand_assignment_provider::events::OnDemandOrderPlaced>()?
		.unwrap();

	let claims_ready = wait_for_para_event(
		para_events.clone(),
		"Broker",
		"ClaimsReady",
		|e: &broker_events::ClaimsReady| e.when == pooled.region_id.begin,
	)
	.await;
	assert_eq!(claims_ready.private_payout, order.spot_price / 2);

	let r = push_tx_hard(
		para_client.clone(),
		&coretime_api::tx().broker().claim_revenue(pooled.region_id, pooled.duration),
		&alice,
	)
	.await;

	let claim_paid = wait_for_para_event(
		para_events.clone(),
		"Broker",
		"RevenueClaimPaid",
		|e: &broker_events::RevenueClaimPaid| e.who == alice_acc,
	)
	.await;

	tokio::time::sleep(std::time::Duration::from_secs(3600)).await;

	Ok(())
}
