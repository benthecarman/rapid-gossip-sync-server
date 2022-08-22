use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::Instant;
use lightning::routing::gossip::NetworkGraph;
use lightning::util::ser::Writeable;
use tokio::sync::mpsc;
use tokio_postgres::NoTls;

use crate::{config, hex_utils, TestLogger};
use crate::types::GossipMessage;

pub(crate) struct GossipPersister {
	gossip_persistence_receiver: mpsc::Receiver<GossipMessage>,
	server_sync_completion_sender: mpsc::Sender<()>,
	network_graph: Arc<NetworkGraph<TestLogger>>,
}

impl GossipPersister {
	pub fn new(server_sync_completion_sender: mpsc::Sender<()>, network_graph: Arc<NetworkGraph<TestLogger>>) -> (Self, mpsc::Sender<GossipMessage>) {
		let (gossip_persistence_sender, gossip_persistence_receiver) =
			mpsc::channel::<GossipMessage>(100);
		(GossipPersister {
			gossip_persistence_receiver,
			server_sync_completion_sender,
			network_graph
		}, gossip_persistence_sender)
	}

	pub(crate) async fn persist_gossip(&mut self) {
		let connection_config = config::db_connection_config();
		let (client, connection) =
			connection_config.connect(NoTls).await.unwrap();

		tokio::spawn(async move {
			if let Err(e) = connection.await {
				panic!("connection error: {}", e);
			}
		});

		{
			// initialize the database
			let initialization = client
				.execute(config::db_config_table_creation_query(), &[])
				.await;
			if let Err(initialization_error) = initialization {
				panic!("db init error: {}", initialization_error);
			}

			let initialization = client
				.execute(
					// TODO: figure out a way to fix the id value without Postgres complaining about
					// its value not being default
					"INSERT INTO config (id, db_schema) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING",
					&[&1, &config::SCHEMA_VERSION]
				).await;
			if let Err(initialization_error) = initialization {
				panic!("db init error: {}", initialization_error);
			}

			let initialization = client
				.execute(config::db_announcement_table_creation_query(), &[])
				.await;
			if let Err(initialization_error) = initialization {
				panic!("db init error: {}", initialization_error);
			}

			let initialization = client
				.execute(
					config::db_channel_update_table_creation_query(),
					&[],
				)
				.await;
			if let Err(initialization_error) = initialization {
				panic!("db init error: {}", initialization_error);
			}

			let initialization = client
				.batch_execute(config::db_index_creation_query())
				.await;
			if let Err(initialization_error) = initialization {
				panic!("db init error: {}", initialization_error);
			}
		}

		// print log statement every 10,000 messages
		let mut persistence_log_threshold = 10000;
		let mut i = 0u32;
		let mut server_sync_completion_sent = false;
		let mut latest_graph_cache_time = Instant::now();
		// TODO: it would be nice to have some sort of timeout here so after 10 seconds of
		// inactivity, some sort of message could be broadcast signaling the activation of request
		// processing
		while let Some(gossip_message) = &self.gossip_persistence_receiver.recv().await {
			i += 1; // count the persisted gossip messages

			if i == 1 || i % persistence_log_threshold == 0 {
				println!("Persisting gossip message #{}", i);
			}

			// has it been ten minutes? Just cache it
			if latest_graph_cache_time.elapsed().as_secs() >= 600 {
				self.persist_network_graph();
				latest_graph_cache_time = Instant::now();
			}

			match &gossip_message {
				GossipMessage::InitialSyncComplete => {
					// signal to the server that it may now serve dynamic responses and calculate
					// snapshots
					// we take this detour through the persister to ensure that all previous
					// messages have already been persisted to the database
					println!("Persister caught up with gossip!");
					i -= 1; // this wasn't an actual gossip message that needed persisting
					persistence_log_threshold = 50;
					if !server_sync_completion_sent {
						server_sync_completion_sent = true;
						self.server_sync_completion_sender.send(()).await.unwrap();
						println!("Server has been notified of persistence completion.");
					}
				}
				GossipMessage::ChannelAnnouncement(announcement) => {

					let scid = announcement.contents.short_channel_id;
					let scid_hex = hex_utils::hex_str(&scid.to_be_bytes());
					// scid is 8 bytes
					// block height is the first three bytes
					// to obtain block height, shift scid right by 5 bytes (40 bits)
					let block_height = (scid >> 5 * 8) as i32;
					let chain_hash = announcement.contents.chain_hash.as_ref();
					let chain_hash_hex = hex_utils::hex_str(chain_hash);

					// start with the type prefix, which is already known a priori
					let mut announcement_signed = Vec::new(); // vec![1, 0];
					announcement.write(&mut announcement_signed).unwrap();

					let result = client
						.execute("INSERT INTO channel_announcements (\
							short_channel_id, \
							block_height, \
							chain_hash, \
							announcement_signed \
						) VALUES ($1, $2, $3, $4) ON CONFLICT (short_channel_id) DO NOTHING", &[
							&scid_hex,
							&block_height,
							&chain_hash_hex,
							&announcement_signed
						]).await;
					if result.is_err() {
						panic!("error: {}", result.err().unwrap());
					}
				}
				GossipMessage::ChannelUpdate(update) => {
					let scid = update.contents.short_channel_id;
					let scid_hex = hex_utils::hex_str(&scid.to_be_bytes());

					let chain_hash = update.contents.chain_hash.as_ref();
					let chain_hash_hex = hex_utils::hex_str(chain_hash);

					let timestamp = update.contents.timestamp as i64;

					let channel_flags = update.contents.flags as i32;
					let direction = channel_flags & 1;
					let disable = (channel_flags & 2) > 0;

					let composite_index = format!("{}:{}:{}", scid_hex, timestamp, direction);

					let cltv_expiry_delta = update.contents.cltv_expiry_delta as i32;
					let htlc_minimum_msat = update.contents.htlc_minimum_msat as i64;
					let fee_base_msat = update.contents.fee_base_msat as i32;
					let fee_proportional_millionths =
						update.contents.fee_proportional_millionths as i32;
					let htlc_maximum_msat = update.contents.htlc_maximum_msat as i64;

					// start with the type prefix, which is already known a priori
					let mut update_signed = Vec::new(); // vec![1, 2];
					update.write(&mut update_signed).unwrap();

					let result = client
						.execute("INSERT INTO channel_updates (\
							composite_index, \
							chain_hash, \
							short_channel_id, \
							timestamp, \
							channel_flags, \
							direction, \
							disable, \
							cltv_expiry_delta, \
							htlc_minimum_msat, \
							fee_base_msat, \
							fee_proportional_millionths, \
							htlc_maximum_msat, \
							blob_signed \
						) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)  ON CONFLICT (composite_index) DO NOTHING", &[
							&composite_index,
							&chain_hash_hex,
							&scid_hex,
							&timestamp,
							&channel_flags,
							&direction,
							&disable,
							&cltv_expiry_delta,
							&htlc_minimum_msat,
							&fee_base_msat,
							&fee_proportional_millionths,
							&htlc_maximum_msat,
							&update_signed
						]).await;
					if result.is_err() {
						panic!("error: {}", result.err().unwrap());
					}
				}
			}
		}
	}

	fn persist_network_graph(&self) {
		println!("Caching network graph…");
		let cache_path = config::network_graph_cache_path();
		let file = OpenOptions::new()
			.create(true)
			.write(true)
			.truncate(true)
			.open(&cache_path)
			.unwrap();
		self.network_graph.remove_stale_channels();
		let mut writer = BufWriter::new(file);
		self.network_graph.write(&mut writer).unwrap();
		writer.flush().unwrap();
		println!("Cached network graph!");
	}
}
