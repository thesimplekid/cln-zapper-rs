use anyhow::{anyhow, Result};
use cln_plugin::options::{ConfigOption, Value};
use cln_rpc::model::{WaitanyinvoiceRequest, WaitanyinvoiceResponse};
use nostr_sdk::{EventBuilder, Keys, Tag};
use std::path::PathBuf;
use tokio::io::{stdin, stdout};

use nostr_sdk::event::Event;
use nostr_sdk::prelude::*;

use std::string::String;

use log::warn;

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

    let relays = vec![nostr_relay];

    let keys = Keys::from_sk_str(&nostr_sec_key)?;

    loop {
        let invoice = match wait_for_invoice(&rpc_socket).await {
            Ok(invoice) => invoice,
            Err(err) => {
                warn!("Error while waiting for invoice: {}", err);
                continue;
            }
        };

        let zap_request_info = match decode_zapreq(&invoice.description) {
            Ok(info) => info,
            Err(err) => {
                warn!("Error while decoding zap request info: {}", err);
                continue;
            }
        };

        let zap_note = match create_zap_note(&keys, zap_request_info.clone(), invoice) {
            Ok(note) => note,
            Err(err) => {
                warn!("Error while creating zap note: {}", err);
                continue;
            }
        };

        let mut relays = relays.clone();
        relays.extend(
            zap_request_info
                .relays
                .iter()
                .map(|r| r.clone().as_vec()[0].clone()),
        );

        if let Err(err) = broadcast_zap_note(&keys, relays, zap_note).await {
            warn!("Error while broadcasting zap note: {}", err);
            continue;
        };
    }
}

async fn broadcast_zap_note(keys: &Keys, relays: Vec<String>, zap_note: Event) -> Result<()> {
    // Create new client
    let client = Client::new(keys);

    // Add relays
    for relay in relays {
        client.add_relay(relay, None).await?;
    }

    client.send_event(zap_note).await?;

    Ok(())
}

async fn wait_for_invoice(socket_addr: &PathBuf) -> Result<WaitanyinvoiceResponse> {
    let mut cln_client = cln_rpc::ClnRpc::new(&socket_addr).await?;

    let invoice = cln_client
        .call(cln_rpc::Request::WaitAnyInvoice(WaitanyinvoiceRequest {
            timeout: None,
            lastpay_index: None,
        }))
        .await?;

    let invoice: WaitanyinvoiceResponse = invoice.try_into().unwrap();
    Ok(invoice)
}

#[derive(Clone)]
struct ZapRequestInfo {
    p: Tag,
    e: Option<Tag>,
    relays: Vec<Tag>,
}

fn decode_zapreq(description: &str) -> Result<ZapRequestInfo> {
    let zap_request: Event = serde_json::from_str(description)?;

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
    let relays: Vec<Tag> = zap_request
        .tags
        .iter()
        .filter(|t| matches!(t, Tag::Relay(_)))
        .cloned()
        .collect();

    Ok(ZapRequestInfo {
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
        nostr_sdk::prelude::TagKind::Custom("bolt11".to_string()),
        vec![bolt11],
    ));

    // Add preimage tag
    tags.push(Tag::Generic(
        nostr_sdk::prelude::TagKind::Custom("preimage".to_string()),
        vec![String::from_utf8_lossy(&preimage.to_vec()).to_string()],
    ));

    // Add description tag
    tags.push(Tag::Generic(
        nostr_sdk::prelude::TagKind::Custom("description".to_string()),
        vec![invoice.description],
    ));

    Ok(EventBuilder::new(nostr_sdk::Kind::Zap, "".to_string(), &tags).to_event(keys)?)
}
