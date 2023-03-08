use anyhow::{anyhow, Result};
use cln_plugin::options::{ConfigOption, Value};
use cln_rpc::model::{WaitanyinvoiceRequest, WaitanyinvoiceResponse, WaitanyinvoiceStatus};
use nostr_sdk::{EventBuilder, Keys, Tag};
use std::path::PathBuf;
use tokio::io::{stdin, stdout};

use nostr_sdk::event::Event;
use nostr_sdk::prelude::*;

use std::string::String;

use log::{info, warn};

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

    let relays = vec![
        nostr_relay,
        "wss://relay.oxtr.com".to_string(),
        "wss://relay.damus.io".to_string(),
        "wss://relay.utxo.one".to_string(),
    ];

    let keys = Keys::from_sk_str(&nostr_sec_key)?;

    let mut last_pay_index = 1;
    loop {
        let invoice = match wait_for_invoice(&rpc_socket, last_pay_index).await {
            Ok(invoice) => invoice,
            Err(err) => {
                warn!("Error while waiting for invoice: {}", err);
                continue;
            }
        };

        info!("Invoice: {:?}", invoice);
        match &invoice.status {
            WaitanyinvoiceStatus::EXPIRED => continue,
            WaitanyinvoiceStatus::PAID => last_pay_index = invoice.pay_index.unwrap() + 1,
        }

        let zap_request_info = match decode_zapreq(&invoice.description) {
            Ok(info) => info,
            Err(err) => {
                warn!("Error while decoding zap request info: {:?}", err);
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

        if let Err(err) = broadcast_zap_note(&keys, relays, zap_note.clone()).await {
            warn!("Error while broadcasting zap note: {}", err);
            continue;
        };
        info!("Broadcasted: {:?}", zap_note.as_json());
    }
}

async fn broadcast_zap_note(keys: &Keys, relays: Vec<String>, zap_note: Event) -> Result<()> {
    // Create new client
    let client = Client::new(keys);

    // Add relays
    for relay in &relays {
        client.add_relay(relay, None).await?;
    }
    info!("relays: {:?}", relays);

    zap_note.verify()?;

    info!("{:#?}", zap_note);

    client.send_event(zap_note).await?;
        // Handle notifications
        let mut notifications = client.notifications();
        while let Ok(notification) = notifications.recv().await {
            info!("{notification:?}");
        }

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

#[derive(Clone, Debug)]
struct ZapRequestInfo {
    zap_request: Event,
    p: Tag,
    e: Option<Tag>,
    relays: Vec<Tag>,
}

fn decode_zapreq(description: &str) -> Result<ZapRequestInfo> {
    info!("String: {description:?}");
    let description: Vec<Vec<String>> = serde_json::from_str(description)?;
    info!("{description:?}");
    let zap_request: Event = description
        .iter()
        .find(|i| i[0] == "text/plain")
        .map(|i| serde_json::from_str(&i[1]))
        .transpose()?
        .unwrap();
    info!("zap_request: {:?}", zap_request);

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
        nostr_sdk::prelude::TagKind::Custom("bolt11".to_string()),
        vec![bolt11],
    ));

    //// Add preimage tag
    //tags.push(Tag::Generic(
    //    nostr_sdk::prelude::TagKind::Custom("preimage".to_string()),
    //    vec![String::from_utf8_lossy(&preimage.to_vec()).to_string()],
    //));

    // Add description tag
    tags.push(Tag::Generic(
        nostr_sdk::prelude::TagKind::Custom("description".to_string()),
        vec![zap_request_info.zap_request.as_json().replace("/", "")],
    ));

    Ok(EventBuilder::new(nostr_sdk::Kind::Zap, "".to_string(), &tags).to_event(keys)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_zap_req() {
        let t =  "[[\\\"text/plain\\\",\\\"{\\\\\\\"id\\\\\\\":\\\\\\\"c0c954af17d7b9a5fe89ffb57b03ab9891ab9cff3ae4a9afebdfa1fc5a4a3ac0\\\\\\\",\\\\\\\"pubkey\\\\\\\":\\\\\\\"04918dfc36c93e7db6cc0d60f37e1522f1c36b64d3f4b424c532d7c595febbc5\\\\\\\",\\\\\\\"created_at\\\\\\\":1678324657,\\\\\\\"kind\\\\\\\":9734,\\\\\\\"tags\\\\\\\":[[\\\\\\\"p\\\\\\\",\\\\\\\"04918dfc36c93e7db6cc0d60f37e1522f1c36b64d3f4b424c532d7c595febbc5\\\\\\\"],[\\\\\\\"relays\\\\\\\",\\\\\\\"wss://relay.damus.io\\\\\\\",\\\\\\\"wss://nostr.wine\\\\\\\",\\\\\\\"wss://nostr-pub.wellorder.net\\\\\\\",\\\\\\\"wss://relay.orangepill.dev\\\\\\\",\\\\\\\"wss://relay.nostr.band\\\\\\\",\\\\\\\"wss://relay.utxo.one\\\\\\\",\\\\\\\"wss://dublin.saoirse.dev\\\\\\\",\\\\\\\"wss://brb.io\\\\\\\",\\\\\\\"wss://nostr.zebedee.cloud\\\\\\\",\\\\\\\"wss://mutinywallet.com\\\\\\\",\\\\\\\"wss://eden.nostr.land\\\\\\\",\\\\\\\"wss://nostr.oxtr.dev\\\\\\\",\\\\\\\"wss://nostr.milou.lol\\\\\\\"],[\\\\\\\"amount\\\\\\\",\\\\\\\"500000\\\\\\\"]],\\\\\\\"content\\\\\\\":\\\\\\\"\\\\\\\",\\\\\\\"sig\\\\\\\":\\\\\\\"c9e5da6b7cb3c204e72518d543eda2f948a345abf9e470e917851dc2c54ae53de7ef536e85e9abe746460260e530ba7505fc3c79c54cae598c6f80fc0681ade8\\\\\\\"}\\\"]]";
        //let zap_req = "[[\\\"text/plain\\\",\\\"{\\\"id\\\":\\\"8afbee1ef9ca4289f4c160b584cc14706af69081ad18577c6504953eba88eb1e\\\",\\\"pubkey\\\":\\\"04918dfc36c93e7db6cc0d60f37e1522f1c36b64d3f4b424c532d7c595febbc5\\\",\\\"created_at\\\":1678311783,\\\"kind\\\":9734,\\\"tags\\\":[[\\\"e\\\",\\\"d2b74f7f6344ac504c1c20d6d30c90eb9706820dc48ae2c9931f2c4881f66295\\\"],[\\\"p\\\",\\\"04918dfc36c93e7db6cc0d60f37e1522f1c36b64d3f4b424c532d7c595febbc5\\\"],[\\\"relays\\\",\\\"wss://relay.nostr.band\\\",\\\"wss://relay.damus.io\\\",\\\"wss://nostr.wine\\\",\\\"wss://relay.orangepill.dev\\\",\\\"wss://brb.io\\\",\\\"wss://nostr.milou.lol\\\",\\\"wss://nostr-pub.wellorder.net\\\",\\\"wss://dublin.saoirse.dev\\\",\\\"wss://relay.utxo.one\\\",\\\"wss://nostr.oxtr.dev\\\",\\\"wss://eden.nostr.land\\\",\\\"wss://mutinywallet.com\\\",\\\"wss://nostr.africa.gives\\\",\\\"wss://nostr.zebedee.cloud\\\"],[\\\"amount\\\",\\\"500000\\\"]],\\\"content\\\":\\\"\\\",\\\"sig\\\":\\\"aa787e6636efebdbb725fbe24bb6ae54626db29c60ae905802ccb7249ee3dfa18ec877137fd5d317858239958fd4f7a864daa6a7c11ba1d9366ea1eebd414faf\\\"}\\\"]]";

        let zap_re_info = decode_zapreq(t).unwrap();

        println!("{zap_re_info:?}");
    }
}
