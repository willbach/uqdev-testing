use std::collections::HashMap;

use anyhow::{self, Error};
use serde::{Deserialize, Serialize};
use uqbar_process_lib::{
    await_message, get_payload,
    http::{
        send_response, send_ws_message, serve_ui, HttpServerRequest, IncomingHttpRequest,
        StatusCode, WsMessageType, bind_http_path,
    },
    print_to_terminal, Address, Message, Payload, ProcessId, Request, Response,
};

wit_bindgen::generate!({
    path: "wit",
    world: "process",
    exports: {
        world: Component,
    },
});

#[derive(Debug, Serialize, Deserialize)]
enum ChatRequest {
    Send { target: String, message: String },
    History,
}

#[derive(Debug, Serialize, Deserialize)]
enum ChatResponse {
    Ack,
    History { messages: MessageArchive },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ChatMessage {
    author: String,
    content: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct NewMessage {
    chat: String,
    author: String,
    content: String,
}

type MessageArchive = HashMap<String, Vec<ChatMessage>>;

fn handle_http_server_request(
    our: &Address,
    message_archive: &mut MessageArchive,
    source: &Address,
    ipc: &[u8],
    channel_id: &mut u32,
) -> anyhow::Result<()> {
    let Ok(server_request) = serde_json::from_slice::<HttpServerRequest>(ipc) else {
        return Ok(());
    };

    match server_request {
        HttpServerRequest::WebSocketOpen(new_channel_id) => {
            *channel_id = new_channel_id;
        }
        HttpServerRequest::WebSocketPush { .. } => {
            let Some(payload) = get_payload() else {
                return Ok(());
            };

            handle_chat_request(
                our,
                message_archive,
                channel_id,
                source,
                &payload.bytes,
                false,
            )?;
        }
        HttpServerRequest::WebSocketClose(_channel_id) => {}
        HttpServerRequest::Http(IncomingHttpRequest { method, .. }) => {
            match method.as_str() {
                // Get all messages
                "GET" => {
                    send_response(
                        StatusCode::OK,
                        None,
                        serde_json::to_vec(&ChatResponse::History {
                            messages: message_archive.clone(),
                        })
                        .unwrap(),
                    )?;
                }
                // Send a message
                "POST" => {
                    let Some(payload) = get_payload() else {
                        return Ok(());
                    };
                    handle_chat_request(
                        our,
                        message_archive,
                        channel_id,
                        source,
                        &payload.bytes,
                        true,
                    )?;

                    // Send an http response via the http server
                    send_response(StatusCode::CREATED, None, vec![])?;
                }
                _ => {
                    // Method not allowed
                    send_response(StatusCode::METHOD_NOT_ALLOWED, None, vec![])?;
                }
            }
        }
    };

    Ok(())
}

fn handle_chat_request(
    our: &Address,
    message_archive: &mut MessageArchive,
    channel_id: &mut u32,
    source: &Address,
    ipc: &[u8],
    is_http: bool,
) -> anyhow::Result<()> {
    let Ok(chat_request) = serde_json::from_slice::<ChatRequest>(ipc) else {
        return Ok(());
    };

    match chat_request {
        ChatRequest::Send {
            ref target,
            ref message,
        } => {
            // counterparty will be the other node in the chat with us
            let (counterparty, author) = if target == &our.node {
                (&source.node, source.node.clone())
            } else {
                (target, our.node.clone())
            };

            // If the target is not us, send a request to the target
            if target != &our.node {
                print_to_terminal(0, &format!("new message from {}: {}", source.node, message));

                let _ = Request::new()
                    .target(Address {
                        node: target.clone(),
                        process: ProcessId::from_str("testing:testing:template.uq")?,
                    })
                    .ipc(ipc)
                    .send_and_await_response(5)?
                    .unwrap();
            }

            // Retreive the message archive for the counterparty, or create a new one if it doesn't exist
            let messages = match message_archive.get_mut(counterparty) {
                Some(messages) => messages,
                None => {
                    message_archive.insert(counterparty.clone(), Vec::new());
                    message_archive.get_mut(counterparty).unwrap()
                }
            };

            let new_message = ChatMessage {
                author: author.clone(),
                content: message.clone(),
            };

            // If this is an HTTP request, handle the response in the calling function
            if is_http {
                // Add the new message to the archive
                messages.push(new_message);
                return Ok(());
            }

            // If this is not an HTTP request, send a response to the other node
            Response::new()
                .ipc(serde_json::to_vec(&ChatResponse::Ack).unwrap())
                .send()
                .unwrap();

            // Add the new message to the archive
            messages.push(new_message);

            // Generate a Payload for the new message
            let payload = Payload {
                mime: Some("application/json".to_string()),
                bytes: serde_json::json!({
                    "NewMessage": NewMessage {
                        chat: counterparty.clone(),
                        author,
                        content: message.clone(),
                    }
                })
                .to_string()
                .as_bytes()
                .to_vec(),
            };

            // Send a WebSocket message to the http server in order to update the UI
            send_ws_message(our.node.clone(), channel_id.clone(), WsMessageType::Text, payload)?;
        }
        ChatRequest::History => {
            // If this is an HTTP request, send a response to the http server

            Response::new()
                .ipc(
                    serde_json::to_vec(&ChatResponse::History {
                        messages: message_archive.clone(),
                    })
                    .unwrap(),
                )
                .send()
                .unwrap();
        }
    };

    Ok(())
}

fn handle_message(
    our: &Address,
    message_archive: &mut MessageArchive,
    channel_id: &mut u32,
) -> anyhow::Result<()> {
    let message = await_message().unwrap();

    // This is for serving static assets dynamically
    // let ipc = message.ipc();
    // if let Ok(request) = serde_json::from_slice::<HttpServerRequest>(ipc) {
    //     match request {
    //         HttpServerRequest::Http(IncomingHttpRequest { raw_path, .. }) => {
    //             if raw_path.contains(&format!("/{}/assets/", our.process.to_string())) {
    //                 return handle_ui_asset_request(our, "ui", &raw_path);
    //             }
    //         }
    //         _ => {}
    //     }
    // }

    match message {
        Message::Response { .. } => {
            print_to_terminal(0, &format!("testing: got response - {:?}", message));
            return Ok(());
        }
        Message::Request {
            ref source,
            ref ipc,
            ..
        } => {
            // Requests that come from other nodes running this app
            handle_chat_request(our, message_archive, channel_id, source, &ipc, false)?;
            // Requests that come from our http server
            handle_http_server_request(our, message_archive, source, ipc, channel_id)?;
        }
    }

    Ok(())
}

struct Component;
impl Guest for Component {
    fn init(our: String) {
        print_to_terminal(0, "testing: begin");

        let our = Address::from_str(&our).unwrap();
        let mut message_archive: MessageArchive = HashMap::new();
        let mut channel_id = 0;

        // If you have limited asset files, use serve_ui
        serve_ui(&our, "ui").unwrap();

        // If you have asset files > 100 MB or so, use serve_index_html and bind_http_path, and then handle_ui_asset_request in your request handler
        // Note that the bound path (like "/assets/*") must be the same as the path that the assets are referenced from in the index.html file
        // serve_index_html(&our, "ui").unwrap();
        // bind_http_path("/assets/*", true, false).unwrap();

        // Bind HTTP path for serving messages
        bind_http_path("/messages", true, false).unwrap();

        loop {
            match handle_message(&our, &mut message_archive, &mut channel_id) {
                Ok(()) => {}
                Err(e) => {
                    print_to_terminal(0, format!("testing: error: {:?}", e,).as_str());
                }
            };
        }
    }
}