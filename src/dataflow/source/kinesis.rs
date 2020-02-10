use std::cell::RefCell;
use std::rc::Rc;

use crate::server::TimestampHistories;
use dataflow_types::{KinesisSourceConnector, Timestamp};
use timely::dataflow::{Scope, Stream};

use super::util::source;
use super::{SourceStatus, SourceToken};
use expr::SourceInstanceId;

// Kinesis stuff
use bytes::Bytes;
use rusoto_core::{HttpClient, Region, RusotoError};
use rusoto_credential::StaticProvider;
use rusoto_kinesis::{
    Consumer, GetRecordsInput, GetShardIteratorInput, Kinesis, KinesisClient, ListShardsInput,
    ListStreamConsumersInput, PutRecordInput, PutRecordsInput, PutRecordsRequestEntry,
    RegisterStreamConsumerError, RegisterStreamConsumerInput, RegisterStreamConsumerOutput, Shard,
    StartingPosition, SubscribeToShardInput,
};
use std::thread;
use std::time::Duration;
use timely::dataflow::operators::Capability;

pub fn kinesis<G>(
    scope: &G,
    name: String,
    connector: KinesisSourceConnector,
    id: SourceInstanceId,
    advance_timestamp: bool,
    timestamp_histories: TimestampHistories,
    timestamp_tx: Rc<RefCell<Vec<SourceInstanceId>>>,
) -> (Stream<G, (Vec<u8>, Option<i64>)>, Option<SourceToken>)
where
    G: Scope<Timestamp = Timestamp>,
{
    let KinesisSourceConnector {
        arn,
        access_key,
        secret_access_key,
        region,
    } = connector.clone();

    let ts = {
        let prev = timestamp_histories.borrow_mut().insert(id.clone(), vec![]);
        assert!(prev.is_none());
        timestamp_tx.as_ref().borrow_mut().push(id);
        Some(timestamp_tx)
    };

    let (stream, capability) = source(id, ts, scope, &name.clone(), move |info| {
        let activator = scope.activator_for(&info.address[..]);
        let provider = StaticProvider::new(
            String::from(access_key),
            String::from(secret_access_key),
            None,
            None,
        );
        let request_dispatcher = HttpClient::new().unwrap();

        // todo: actually use region -- don't hard code. will need string->region
        let client = KinesisClient::new_with(request_dispatcher, provider, Region::UsEast2);
        let list_consumers_input = ListStreamConsumersInput {
            max_results: None,
            next_token: None,
            stream_arn: arn.clone(),
            stream_creation_timestamp: None,
        };
        let consumers = client
            .list_stream_consumers(list_consumers_input)
            .sync()
            .unwrap();
        // Get a registered Consumer
        let consumer: Option<Consumer> = match consumers.consumers {
            Some(consumers) => {
                if !consumers.is_empty() {
                    Some(consumers[0].clone()) // todo: fix
                } else {
                    // re-register.
                    // Only do this once.
                    //    ResourceInUse(
                    //        "Consumer materialize-test under stream jessica-test already exists for account 231523697266.",
                    //    ),
                    println!("registering our consumer");
                    let register_input = RegisterStreamConsumerInput {
                        consumer_name: String::from("materialize-test"),
                        stream_arn: arn.clone(),
                    };
                    match client.register_stream_consumer(register_input).sync() {
                        Ok(RegisterStreamConsumerOutput { consumer }) => Some(consumer),
                        _ => None,
                    }
                }
            }
            None => None,
        };

        // Subscribe to shard.
        match consumer {
            Some(consumer) => {
                let subscribe_to_shard_input = SubscribeToShardInput {
                    consumer_arn: consumer.consumer_arn,
                    shard_id: String::from("1"),
                    starting_position: StartingPosition::default(),
                };
                client.subscribe_to_shard(subscribe_to_shard_input);
                println!("subscribed");
            }
            None => println!("wouldn't be able to do anything here...."),
        }

        let list_shards = ListShardsInput {
            exclusive_start_shard_id: None,
            max_results: None,
            next_token: None,
            stream_creation_timestamp: None,
            stream_name: Some(String::from("jessica-test")),
        };
        let shards = client.list_shards(list_shards).sync().unwrap();
        let shard_iterator: Option<String> = match shards.shards {
            Some(shards) => {
                let shard = shards[0].clone(); //todo: update
                let get_shard_iterator = GetShardIteratorInput {
                    shard_id: shard.shard_id.clone(),
                    shard_iterator_type: String::from("AT_SEQUENCE_NUMBER"),
                    starting_sequence_number: Some(
                        shard.sequence_number_range.starting_sequence_number,
                    ),
                    stream_name: String::from("jessica-test"),
                    timestamp: None,
                };
                client
                    .get_shard_iterator(get_shard_iterator)
                    .sync()
                    .unwrap()
                    .shard_iterator
            }
            None => None,
        };

        // Index of the last offset that we have already processed
        // todo: this should be the starting sequence number of a shard,
        // then updated?
        let mut last_processed_offset: i64 = -1; // todo: update type?
                                                 //        let mut last_sequence_number = shard.sequence_number_range.starting_sequence_number;

        move |cap, output| {
            // Check if the capability can be downgraded (this is independent of whether
            // there are new messages that can be processed)
            downgrade_capability(&id, cap, last_processed_offset, &timestamp_histories);

            match shard_iterator.clone() {
                Some(mut iterator) => {
                    loop {
                        let get_records_input = GetRecordsInput {
                            limit: None,
                            shard_iterator: iterator,
                        };
                        let records = client.get_records(get_records_input).sync().unwrap();
                        dbg!(&records.millis_behind_latest);
                        if !records.records.is_empty() {
                            dbg!(&records.records);
                            for record in records.records {
                                let timestamp = match record.approximate_arrival_timestamp {
                                    Some(t) => {
                                        let new_t = t as i64;
                                        last_processed_offset = new_t;
                                        Some(t as i64)
                                    } // todo: is this okay?
                                    None => None,
                                };
                                //                            last_sequence_number = iterator;

                                let message = record.data;
                                output.session(&cap).give((
                                    message.to_vec(),
                                    timestamp, // todo: is this okay?
                                ));
                                println!("wrote something to output!, checking to downgrade");
                                downgrade_capability(
                                    &id,
                                    cap,
                                    last_processed_offset,
                                    &timestamp_histories,
                                );
                            }
                        }

                        match records.next_shard_iterator {
                            Some(next) => {
                                iterator = next;
                            }
                            None => {
                                println!("done!");
                                break;
                            }
                        }
                        match records.millis_behind_latest {
                            Some(millis) => {
                                if millis == 0 {
                                    println!("caught up!");
                                    break;
                                }
                            }
                            None => {
                                println!("break!");
                                break;
                            } //todo: is this right??
                        }
                        thread::sleep(Duration::from_millis(500));
                    }
                }
                None => println!("no shard iterator"),
            }
            // Ensure that we poll kafka more often than the eviction timeout
            // todo: what is the Kinesis timeout??
            activator.activate_after(Duration::from_secs(10));
            SourceStatus::Alive
        }
    });
    (stream, Some(capability))
}

/// Timestamp history map is of format [(ts1, offset1), (ts2, offset2)].
/// All messages in interval [0,offset1] get assigned ts1, all messages in interval [offset1+1,offset2]
/// get assigned ts2, etc.
/// When receive message with offset1, it is safe to downgrade the capability to the next
/// timestamp, which is either
/// 1) the timestamp associated with the next highest offset if it exists
/// 2) max(timestamp, offset1) + 1. The timestamp_history map can contain multiple timestamps for
/// the same offset. We pick the greatest one + 1
/// (the next message we generate will necessarily have timestamp timestamp + 1)
fn downgrade_capability(
    id: &SourceInstanceId,
    cap: &mut Capability<Timestamp>,
    last_processed_offset: i64,
    timestamp_histories: &TimestampHistories,
) {
    println!("in downgrade_capability");
    match timestamp_histories.borrow_mut().get_mut(id) {
        None => {
            println!("got none");
        }
        Some(entries) => {
            while let Some((ts, offset)) = entries.first() {
                let next_ts = ts + 1;
                if last_processed_offset == *offset {
                    println!("last processed offset == offset");
                    entries.remove(0);
                    cap.downgrade(&next_ts);
                } else {
                    println!(
                        "offset {}, last processed offset {}",
                        offset, last_processed_offset
                    );
                    // Offset isn't at a timestamp boundary, we take no action
                    break;
                }
            }
            println!("in downgrade, but no entries.");
        }
    }
}
