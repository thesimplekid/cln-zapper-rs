use anyhow::{anyhow, Result};
use cln_plugin::options::{ConfigOption, Value};
use cln_rpc::model::{WaitanyinvoiceRequest, WaitanyinvoiceResponse};
use futures::{Stream, StreamExt};
use log::{debug, warn};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{stdin, stdout};
use tokio::task;
use serde::Serialize;

use nostr::event::Event;
use nostr::prelude::*;

use tungstenite::Message as WsMessage;

use std::string::String;

use std::fs;

use log::info;
use std::collections::HashSet;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let plugin = if let Some(plugin) = cln_plugin::Builder::new(stdin(), stdout())
        .option(ConfigOption::new(
            "clnzapper_nostr_nsec",
            Value::String("".into()),
            "Nsec for publishing nostr notes",
        ))
        // TODO: Would be better to be a list
        .option(ConfigOption::new(
            "clnzapper_nostr_relay",
            Value::String("ws://localhost:8080".to_string()),
            "Default relay to publish to",
        ))
        .dynamic()
        .start(())
        .await?
    {
        plugin
    } else {
        return Ok(());
    };

    let rpc_socket: PathBuf = plugin.configuration().rpc_file.parse()?;

    let nostr_sec_key = plugin
        .option("clnzapper_nostr_nsec")
        .expect("Option is defined")
        .as_str()
        .expect("Option is a string")
        .to_owned();
    let nostr_relay = plugin
        .option("clnzapper_nostr_relay")
        .expect("Option is defined")
        .as_str()
        .expect("Option is a string")
        .to_owned();

    let mut relays = HashSet::new();
    relays.insert(nostr_relay);

    let keys = Keys::from_sk_str(&nostr_sec_key)?;

    let mut invoices = invoice_stream(&rpc_socket).await?;
    while let Some((zap_request_info, invoice)) = invoices.next().await {
        let zap_note = match create_zap_note(&keys, zap_request_info.clone(), invoice) {
            Ok(note) => note,
            Err(err) => {
                info!("Error while creating zap note: {}", err);
                continue;
            }
        };

        info!("Zap Note: {}", zap_note.as_json());

        let mut relays = relays.clone();
        relays.extend(zap_request_info.relays);

        task::spawn(async move {
            let zap_note_id = zap_note.id.to_hex();
            if let Err(err) = broadcast_zap_note(&relays, zap_note).await {
                info!("Error while broadcasting zap note: {}", err);
            };
            info!("Broadcasted: {:?}", zap_note_id);
            info!("To relays: {:?}", relays);
        });
    }

    Ok(())
}

async fn broadcast_zap_note(relays: &HashSet<String>, zap_note: Event) -> Result<()> {
    // Create new client
    // let client = Client::new(keys);
    zap_note.verify()?;
    // info!("Note to broadcast {}", zap_note.as_json());

    // Add relays
    for relay in relays {
        let mut socket = match tungstenite::connect(relay) {
            Ok((s, _)) => s,
            Err(err) => {
                info!("Error connecting to {relay}: {err}");
                continue;
            }
        };
        // Send msg
        let msg = ClientMessage::new_event(zap_note.clone()).as_json();
        socket
            .write_message(WsMessage::Text(msg))
            .expect("Impossible to send message");
    }
    info!("relays: {:?}", relays);

    Ok(())
}

async fn invoice_stream(
    socket_addr: &PathBuf,
) -> Result<impl Stream<Item = (ZapRequestInfo, WaitanyinvoiceResponse)>> {
    let cln_client = cln_rpc::ClnRpc::new(&socket_addr).await?;

    Ok(futures::stream::unfold(
        (cln_client, None),
        |(mut cln_client, mut last_pay_idx)| async move {
            // We loop here since some invoices aren't zaps, in which case we wait for the next one and don't yield
            loop {
                let invoice_res = cln_client
                    .call(cln_rpc::Request::WaitAnyInvoice(WaitanyinvoiceRequest {
                        timeout: None,
                        lastpay_index: last_pay_idx,
                    }))
                    .await;

                let invoice: WaitanyinvoiceResponse = match invoice_res {
                    Ok(invoice) => invoice,
                    Err(e) => {
                        warn!("Error fetching invoice: {e}");
                        // Let's not spam CLN with requests on failure
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        // Retry same reqeuest
                        continue;
                    }
                }
                .try_into()
                .expect("Wrong response from CLN");

                match decode_zapreq(&invoice.description) {
                    Ok(zap) => {
                        let pay_idx = invoice.pay_index;
                        // yield zap
                        break Some(((zap, invoice), (cln_client, pay_idx)));
                    }
                    Err(e) => {
                        debug!(
                            "Error while decoding zap (likely just not a zap invoice): {}",
                            e
                        );
                        // Process next invoice without yielding anything
                        last_pay_idx = invoice.pay_index;
                        continue;
                    }
                }
            }
        },
    )
    .boxed())
}

#[derive(Clone, Debug, Serialize)]
struct ZapRequestInfo {
    zap_request: Event,
    p: Tag,
    e: Option<Tag>,
    relays: HashSet<String>,
}

fn decode_zapreq(description: &str) -> Result<ZapRequestInfo> {
    let zap_request: Event = Event::from_json(description)?;

    // Verify zap request is a valid nostr event
    zap_request.verify()?;

    // Filter to get p tags
    let p_tags: Vec<Tag> = zap_request
        .tags
        .iter()
        .filter(|t| matches!(t, Tag::PubKey(_, _)))
        .cloned()
        .collect();

    // Check there is 1 p tag
    let p_tag = match p_tags.len() {
        1 => p_tags[0].clone(),
        _ => return Err(anyhow!("None or too many p tags")),
    };

    // Filter to get e tags
    let e_tags: Vec<Tag> = zap_request
        .tags
        .iter()
        .filter(|t| matches!(t, Tag::Event(_, _, _)))
        .cloned()
        .collect();

    // Check there is 0 or 1 e tag
    let e_tag = match e_tags.len() {
        0 => None,
        1 => Some(e_tags[0].clone()),
        _ => return Err(anyhow!("Too many e tags")),
    };

    // Filter to get relay tags
    // Im sure the filter and for loop can be done better
    let relays_tag: Vec<Tag> = zap_request
        .tags
        .iter()
        .filter(|t| matches!(t, Tag::Generic(TagKind::Custom(_relays_string), _)))
        .cloned()
        .collect();

    let mut relays = vec![];
    for r in &relays_tag {
        let mut r = r.as_vec();
        if r[0].eq("relays") {
            // println!("{r:?}");
            r.remove(0);
            relays = r;
        }
    }

    let relays: HashSet<String> = relays.iter().cloned().collect();

    // println!("{relays:?}");

    Ok(ZapRequestInfo {
        zap_request,
        p: p_tag,
        e: e_tag,
        relays,
    })
}

fn create_zap_note(
    keys: &Keys,
    zap_request_info: ZapRequestInfo,
    invoice: WaitanyinvoiceResponse,
) -> Result<Event> {
    let mut tags = if zap_request_info.e.is_some() {
        vec![zap_request_info.e.unwrap(), zap_request_info.p]
    } else {
        vec![zap_request_info.p]
    };

    // Check there is a bolt11
    let bolt11 = match invoice.bolt11 {
        Some(bolt11) => bolt11,
        None => return Err(anyhow!("No bolt 11")),
    };

    // Check there is a preimage
    let preimage = match invoice.payment_preimage {
        Some(pre_image) => pre_image,
        None => return Err(anyhow!("No pre image")),
    };

    // Add bolt11 tag
    tags.push(Tag::Generic(
        TagKind::Custom("bolt11".to_string()),
        vec![bolt11],
    ));

    // Add preimage tag
    //tags.push(Tag::Generic(
    //    nostr_sdk::prelude::TagKind::Custom("preimage".to_string()),
    //    vec![String::from_utf8_lossy(&preimage.to_vec()).to_string()],
    //));

    // Add description tag
    // description of bolt11 invoice a JSON encoded zap request
    tags.push(Tag::Generic(
        TagKind::Custom("description".to_string()),
        vec![zap_request_info.zap_request.as_json()],
    ));

    Ok(EventBuilder::new(nostr::Kind::Zap, "".to_string(), &tags).to_event(keys)?)
}
