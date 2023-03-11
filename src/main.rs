use anyhow::{anyhow, Result};
use cln_plugin::options::{ConfigOption, Value};
use cln_rpc::model::{WaitanyinvoiceRequest, WaitanyinvoiceResponse, WaitanyinvoiceStatus};
use nostr::{EventBuilder, Keys, Tag};
use serde::Serialize;
use std::{collections::HashSet, path::PathBuf};
use tokio::io::{stdin, stdout};
use tokio::task;

use nostr::event::Event;
use nostr::prelude::*;

use tungstenite::Message as WsMessage;

use std::string::String;

use std::fs;

use log::info;

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

    /*
    let mut last_pay_index = match read_last_pay_index() {
        Ok(value) => value,
        Err(_) => {
            write_last_pay_index(1)?;
            1
        }
    };
    */

    let mut last_pay_index = 1;

    info!("{last_pay_index}");

    loop {
        info!("Waiting for index: {last_pay_index}");
        let invoice = match wait_for_invoice(&rpc_socket, last_pay_index).await {
            Ok(invoice) => invoice,
            Err(err) => {
                info!("Error while waiting for invoice: {}", err);
                continue;
            }
        };

        info!("Invoice: {:?}", invoice);
        match &invoice.status {
            WaitanyinvoiceStatus::EXPIRED => continue,
            WaitanyinvoiceStatus::PAID => {
                last_pay_index += 1;
                // write_last_pay_index(last_pay_index).ok();
            }
        }

        let zap_request_info = match decode_zapreq(&invoice.description) {
            Ok(info) => info,
            Err(err) => {
                info!("Error while decoding zap request info: {:?}", err);
                continue;
            }
        };

        info!("Zap Request: {zap_request_info:?}");

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

async fn wait_for_invoice(
    socket_addr: &PathBuf,
    lastpay_index: u64,
) -> Result<WaitanyinvoiceResponse> {
    let mut cln_client = cln_rpc::ClnRpc::new(&socket_addr).await?;

    let invoice = cln_client
        .call(cln_rpc::Request::WaitAnyInvoice(WaitanyinvoiceRequest {
            timeout: None,
            lastpay_index: Some(lastpay_index),
        }))
        .await?;

    let invoice: WaitanyinvoiceResponse = invoice.try_into().unwrap();
    Ok(invoice)
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

fn read_last_pay_index() -> Result<u64> {
    let data = fs::read("output.bin").expect("Failed to read file");
    let value = u64::from_ne_bytes(data.as_slice().try_into()?);

    Ok(value)
}

fn write_last_pay_index(last_pay_index: u64) -> Result<()> {
    let data = last_pay_index.to_ne_bytes();

    fs::write("output.bin", &data)?;
    Ok(())
}
