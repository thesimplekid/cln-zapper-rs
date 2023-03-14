use anyhow::{anyhow, Result};
use cln_plugin::options::{ConfigOption, Value};
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

    let last_pay_index = match read_last_pay_index() {
        Ok(idx) => idx,
        Err(_e) => {
            if let Err(e) = write_last_pay_index(0) {
                warn!("Write error: {e}");
            }
            0
        }
    };
    info!("Starting at pay index: {last_pay_index}");

    let mut invoices = invoice_stream(&rpc_socket, Some(last_pay_index)).await?;
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
        info!("Broadcasted: {:?}", zap_note_id);
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
    last_pay_index: Option<u64>,
) -> Result<impl Stream<Item = (ZapRequestInfo, WaitanyinvoiceResponse)>> {
    let cln_client = cln_rpc::ClnRpc::new(&socket_addr).await?;

    Ok(futures::stream::unfold(
        (cln_client, last_pay_index),
        |(mut cln_client, mut last_pay_idx)| async move {
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

                // Process next invoice without yielding anything
                last_pay_idx = invoice.pay_index;
                if let Some(idx) = last_pay_idx {
                    if let Err(e) = write_last_pay_index(idx) {
                        warn!("Could not write index tip: {e}");
                    }
                };

                match decode_zap_req(&invoice.description) {
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
    /// Realys in zap request
    relays: HashSet<String>,
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

    // Filter to get relay tags
    // Im sure the filter and for loop can be done better
    let relays_tag: Vec<Tag> = zap_request
        .tags
        .iter()
        .filter(|t| matches!(t, Tag::Generic(TagKind::Custom(_relays_string), _)))
        .cloned()
        .collect();

    // relays of zap request
    let mut relays = vec![];
    for r in &relays_tag {
        let mut r = r.as_vec();
        if r[0].eq("relays") {
            r.remove(0);
            relays = r;
        }
    }

    let relays: HashSet<String> = relays.iter().cloned().collect();

    Ok(ZapRequestInfo {
        zap_request,
        p: p_tag,
        e: e_tag,
        relays,
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
    tags.push(Tag::Generic(
        TagKind::Custom("bolt11".to_string()),
        vec![bolt11],
    ));

    if let Some(pre_image) = invoice.payment_preimage {
        // Add preimage tag
        // Pre image is optional according to the spec
        tags.push(Tag::Generic(
            TagKind::Custom("preimage".to_string()),
            vec![pre_image.to_vec().to_hex()],
        ));
    }

    // Add description tag
    // description of bolt11 invoice a JSON encoded zap request
    tags.push(Tag::Generic(
        TagKind::Custom("description".to_string()),
        vec![invoice.description],
    ));

    Ok(EventBuilder::new(nostr::Kind::Zap, "".to_string(), &tags).to_event(keys)?)
}

fn index_file_path() -> Result<PathBuf> {
    let mut file_path = match data_dir() {
        Some(path) => path,
        None => return Err(anyhow!("no data dir")),
    };

    file_path.push("cln-zapper");
    file_path.push("last_pay_index");

    Ok(file_path)
}

fn read_last_pay_index() -> Result<u64> {
    let file_path = index_file_path()?;
    let mut file = File::open(file_path)?;
    let mut buffer = [0; 8];

    file.read_exact(&mut buffer)?;
    Ok(u64::from_ne_bytes(buffer))
}

fn write_last_pay_index(last_pay_index: u64) -> Result<()> {
    let file_path = index_file_path()?;

    // Create the directory if it doesn't exist
    if let Some(parent_dir) = file_path.parent() {
        fs::create_dir_all(parent_dir)?;
    }

    let mut file = File::create(&file_path)?;
    file.write_all(&last_pay_index.to_ne_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn save_last_pay_index() {
        let last_pay_index = 42;
        write_last_pay_index(last_pay_index).unwrap();

        let file_last_pay_index = read_last_pay_index().unwrap();

        assert_eq!(last_pay_index, file_last_pay_index);

        let plus = file_last_pay_index + 1;
        println!("{plus}");
        write_last_pay_index(plus).unwrap();

        assert_eq!(plus, read_last_pay_index().unwrap());
    }
}
