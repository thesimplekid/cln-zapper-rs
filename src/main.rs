use anyhow::{anyhow, Result};
use cln_plugin::options::{ConfigOption, Value};
use cln_plugin::Plugin;
use cln_rpc::model::{WaitanyinvoiceRequest, WaitanyinvoiceResponse};
use dirs::data_dir;
use futures::{Stream, StreamExt};
use log::{debug, warn};
use nostr::prelude::hex::ToHex;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{stdin, stdout};

use nostr::event::Event;
use nostr::prelude::*;

use tungstenite::Message as WsMessage;

use std::string::String;

use log::{error, info};
use std::collections::HashSet;

use std::fs::{self, File};
use std::io::{Read, Write};

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
        .option(ConfigOption::new(
            "clnzapper_pay_index_path",
            Value::OptString,
            "Path to pay index",
        ))
        .subscribe("shutdown",
            // Handle CLN `shutdown` if it is sent 
            |plugin: Plugin<()>, _: serde_json::Value| async move {
            info!("Received \"shutdown\" notification from lightningd ... requesting cln_plugin shutdown");
            plugin.shutdown().ok();
            plugin.join().await
        })
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

    // Get pay index file path from cln config if set
    // if not set to default
    let pay_index_path = match plugin.option("clnzapper_pay_index_path") {
        Some(Value::String(path)) => PathBuf::from(path),
        Some(Value::OptString) => index_file_path()?,
        _ => {
            // Something unexpected happened
            warn!("Unexpected index path config");
            index_file_path()?
        }
    };

    info!("Pay index path {pay_index_path:?}");

    let mut relays = HashSet::new();
    relays.insert(nostr_relay);

    let keys = Keys::from_sk_str(&nostr_sec_key)?;

    let last_pay_index = match read_last_pay_index(&pay_index_path) {
        Ok(idx) => idx,
        Err(e) => {
            warn!("Could not read last pay index: {e}");
            if let Err(e) = write_last_pay_index(&pay_index_path, 0) {
                warn!("Write error: {e}");
            }
            0
        }
    };
    info!("Starting at pay index: {last_pay_index}");

    let mut invoices = invoice_stream(&rpc_socket, pay_index_path, Some(last_pay_index)).await?;
    while let Some((zap_request_info, invoice)) = invoices.next().await {
        let zap_note = match create_zap_note(&keys, zap_request_info.clone(), invoice) {
            Ok(note) => note,
            Err(err) => {
                error!("Error while creating zap note: {}", err);
                continue;
            }
        };

        debug!("Zap Note: {}", zap_note.as_json());

        let mut relays = relays.clone();
        relays.extend(zap_request_info.relays);

        let zap_note_id = zap_note.id.to_hex();
        if let Err(err) = broadcast_zap_note(&relays, zap_note).await {
            warn!("Error while broadcasting zap note: {}", err);
        };
        info!("Broadcasted: {}", zap_note_id);
        // info!("To relays: {:?}", relays);
    }

    Ok(())
}

async fn broadcast_zap_note(relays: &HashSet<String>, zap_note: Event) -> Result<()> {
    // Create new client
    zap_note.verify()?;
    // info!("Note to broadcast {}", zap_note.as_json());

    for relay in relays {
        let mut socket = match tungstenite::connect(relay) {
            Ok((s, _)) => s,
            // TODO: the mutiny relay returns an http 200 its getting logged as an error
            Err(err) => {
                warn!("Error connecting to {relay}: {err}");
                continue;
            }
        };

        // Send msg
        let msg = ClientMessage::new_event(zap_note.clone()).as_json();
        socket
            .write_message(WsMessage::Text(msg))
            .expect("Impossible to send message");
    }

    Ok(())
}

async fn invoice_stream(
    socket_addr: &PathBuf,
    pay_index_path: PathBuf,
    last_pay_index: Option<u64>,
) -> Result<impl Stream<Item = (ZapRequestInfo, WaitanyinvoiceResponse)>> {
    let cln_client = cln_rpc::ClnRpc::new(&socket_addr).await?;

    Ok(futures::stream::unfold(
        (cln_client, pay_index_path, last_pay_index),
        |(mut cln_client, pay_index_path, mut last_pay_idx)| async move {
            // We loop here since some invoices aren't zaps, in which case we wait for the next one and don't yield
            loop {
                // info!("Waiting for index: {last_pay_idx:?}");
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
                        // Retry same request
                        continue;
                    }
                }
                .try_into()
                .expect("Wrong response from CLN");

                last_pay_idx = invoice.pay_index;
                if let Some(idx) = last_pay_idx {
                    if let Err(e) = write_last_pay_index(&pay_index_path, idx) {
                        warn!("Could not write index tip: {e}");
                    }
                };

                match decode_zap_req(&invoice.description) {
                    Ok(zap) => {
                        let pay_idx = invoice.pay_index;

                        // If there is an amount tag present in zap request check it matches invoice
                        if let (Some(zap_request_amount), Some(invoice_amount)) =
                            (zap.amount, invoice.amount_msat)
                        {
                            if zap_request_amount.ne(&invoice_amount.msat()) {
                                info!(
                                    "Zap request {} amount does not equal invoice amount {}",
                                    zap.zap_request.id.to_hex(),
                                    invoice.label
                                );
                                // Don't yield wait for next invoice
                                continue;
                            }
                        }

                        // yield zap
                        break Some(((zap, invoice), (cln_client, pay_index_path, pay_idx)));
                    }
                    Err(e) => {
                        // Process next invoice without yielding anything
                        debug!(
                            "Error while decoding zap (likely just not a zap invoice): {}",
                            e
                        );
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
    /// Zap Request Event
    zap_request: Event,
    /// p tag of zap request
    p: Tag,
    /// E tag of zap request if related to event
    e: Option<Tag>,
    /// Relays in zap request
    relays: HashSet<String>,
    /// Amount
    amount: Option<u64>,
}

/// Decode str of JSON zap note
fn decode_zap_req(description: &str) -> Result<ZapRequestInfo> {
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

    let relays: HashSet<String> = zap_request
        .tags
        .iter()
        .filter_map(|tag| match tag {
            Tag::Relays(values) => Some(
                values
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<String>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();

    let amount = zap_request.tags.iter().find_map(|tag| {
        if let Tag::Amount(a) = tag {
            return Some(a.to_owned());
        }
        None
    });

    Ok(ZapRequestInfo {
        zap_request,
        p: p_tag,
        e: e_tag,
        relays,
        amount,
    })
}

/// Create zap note
fn create_zap_note(
    keys: &Keys,
    zap_request_info: ZapRequestInfo,
    invoice: WaitanyinvoiceResponse,
) -> Result<Event> {
    let mut tags = if zap_request_info.e.is_some() {
        vec![zap_request_info.p, zap_request_info.e.unwrap()]
    } else {
        vec![zap_request_info.p]
    };

    // Check there is a bolt11
    let bolt11 = match invoice.bolt11 {
        Some(bolt11) => bolt11,
        None => return Err(anyhow!("No bolt 11")),
    };

    // Add bolt11 tag
    tags.push(Tag::Bolt11(bolt11));

    // Add description tag
    // description of bolt11 invoice a JSON encoded zap request
    tags.push(Tag::Description(invoice.description));

    // Add preimage tag if set
    // Pre image is optional according to the spec
    if let Some(pre_image) = invoice.payment_preimage {
        tags.push(Tag::Preimage(pre_image.to_vec().to_hex()));
    }

    Ok(EventBuilder::new(nostr::Kind::Zap, "".to_string(), &tags).to_event(keys)?)
}

/// Default file path for last pay index tip
fn index_file_path() -> Result<PathBuf> {
    let mut file_path = match data_dir() {
        Some(path) => path,
        None => return Err(anyhow!("no data dir")),
    };

    file_path.push("cln-zapper");
    file_path.push("last_pay_index");

    Ok(file_path)
}

/// Read last pay index tip from file
fn read_last_pay_index(file_path: &PathBuf) -> Result<u64> {
    let mut file = File::open(file_path)?;
    let mut buffer = [0; 8];

    file.read_exact(&mut buffer)?;
    Ok(u64::from_ne_bytes(buffer))
}

/// Write last pay index tip to file
fn write_last_pay_index(file_path: &PathBuf, last_pay_index: u64) -> Result<()> {
    // Create the directory if it doesn't exist
    if let Some(parent_dir) = file_path.parent() {
        fs::create_dir_all(parent_dir)?;
    }

    let mut file = File::create(file_path)?;
    file.write_all(&last_pay_index.to_ne_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {

    use std::str::FromStr;

    use cln_rpc::primitives::Amount;

    use super::*;

    #[test]
    fn test_save_last_pay_index() {
        let path = PathBuf::from("./test/last_index");
        let last_pay_index = 42;
        write_last_pay_index(&path, last_pay_index).unwrap();

        let file_last_pay_index = read_last_pay_index(&path).unwrap();

        assert_eq!(last_pay_index, file_last_pay_index);

        let plus = file_last_pay_index + 1;
        println!("{plus}");
        write_last_pay_index(&path, plus).unwrap();

        assert_eq!(plus, read_last_pay_index(&path).unwrap());
    }

    #[test]
    fn test_create_zap_note() {
        use nostr::Keys;

        let keys =
            Keys::from_sk_str("505fd02741816952ec9a70204221acdd8458906d3e1e0604fef033876c811a8f")
                .unwrap();
        let zap_req = "{\"content\":\"\",\"created_at\":1678734288,\"id\":\"c93b75ff70b07d28287059d750756f93281ac779cd780e7d61b781f9862c5a81\",\"kind\":9734,\"pubkey\":\"04918dfc36c93e7db6cc0d60f37e1522f1c36b64d3f4b424c532d7c595febbc5\",\"sig\":\"512d0a3ec6b9797810272b9dc05cadb7f6d271ff72a183350f643fa761bc37820e877563ddc1c5ef30a549a63115a6e907412a60de1dbe35dd7ea3b431a534ba\",\"tags\":[[\"e\",\"d07f03815931a3767ea91ee9cb3920758cd6dcb4e206ef0f1061f7e3c51f338e\"],[\"p\",\"00003687cecf074d81949ce8b95a860789e2be03925f3d3860ae27573fdc2218\"],[\"relays\",\"wss://nostr.wine\",\"wss://relay.damus.io\",\"wss://relay.orangepill.dev\",\"wss://dublin.saoirse.dev\",\"wss://relay.utxo.one\",\"wss://relay.nostr.band\",\"wss://nostr-pub.wellorder.net\",\"wss://nostr.milou.lol\",\"wss://nostr.oxtr.dev\",\"wss://eden.nostr.land\",\"wss://mutinywallet.com\",\"wss://nostr.zebedee.cloud\",\"wss://brb.io\"],[\"amount\",\"50000\"]]}";

        let zap_req_info = decode_zap_req(zap_req).unwrap();

        let invoice = WaitanyinvoiceResponse { label: "c15c98b0-81fe-4864-a9c5-ffad716d466a".to_string(), description: zap_req.to_string(), payment_hash: sha256::Hash::from_str("83f34c56502833b28dc64b382ef8462c2f5edb19c427fd5456d46bfc5c35914b").unwrap(), status: cln_rpc::model::WaitanyinvoiceStatus::PAID, expires_at: 1687338240, amount_msat: Some(Amount::from_msat(5000)), bolt11: Some("lnbc500n1pjq7u7jsp5n5jth3w6d4wjnjmup0nwlr2xfqthg8leru8yj8cyqf3sszapfxeqpp5s0e5c4js9qem9rwxfvuza7zx9sh4akcecsnl64zk634lchp4j99shp5ctnx2g7vddpve39pa35f70d4yua7fypfqjepcygq938ev86ekd7sxqyjw5qcqpjrzjqvhxqvs0ulx0mf5gp6x2vw047capck4pxqnsjv0gg8a4zaegej6gxzlgzuqqttgqqyqqqqqqqqqqqqqqyg9qyysgqs80g00rantwaay8g6wwev33v7xgtu8qkmq4hflgs93ygrxccry6qlhksdd0497pusvlsx3emk0hj5ghecxf6pw84tgxf99r5jg7mjrgpammhml".to_string()), bolt12: None, pay_index: Some(1), amount_received_msat: Some(Amount::from_msat(50000)), paid_at: Some(1687251840), payment_preimage: None};

        let zap_note = create_zap_note(&keys, zap_req_info, invoice.clone()).unwrap();

        zap_note.verify().unwrap();

        let zap_req: serde_json::Value = serde_json::from_str(zap_req).unwrap();

        let zap_req_hash = sha256::Hash::hash(zap_req.to_string().as_bytes());

        let invoice_des_has = sha256::Hash::hash(invoice.description.as_bytes());

        println!("hash: {}", invoice_des_has);

        assert_eq!(zap_req_hash, invoice_des_has);
    }
}
