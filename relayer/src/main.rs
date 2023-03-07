mod rpc_conf;
mod signing;

#[cfg(test)]
use axum::Json;
use axum::{extract, http::StatusCode, response::IntoResponse, Router, routing::post};
use config::{Config, File};
#[cfg(test)]
use near_crypto::{KeyType, PublicKey, Signature};
use near_jsonrpc_client::methods::broadcast_tx_commit::RpcBroadcastTxCommitRequest;
#[cfg(test)]
use near_primitives::borsh::BorshSerialize;
use near_primitives::borsh::BorshDeserialize;
#[cfg(test)]
use near_primitives::delegate_action::{DelegateAction, NonDelegateAction};
use near_primitives::delegate_action::SignedDelegateAction;
#[cfg(test)]
use near_primitives::transaction::{CreateAccountAction, TransferAction};
use near_primitives::transaction::{Action, Transaction};
#[cfg(test)]
use near_primitives::types::{BlockHeight, Nonce};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::net::SocketAddr;
use near_primitives::types::{BlockReference, Finality};
//use tower_http::{classify::ServerErrorsFailureClass, trace::TraceLayer};
use tracing::{debug, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use rpc_conf::rpc_transaction_error;
use signing::sign_transaction;
use crate::rpc_conf::RPCConfig;


// load config from toml and setup json rpc client
static LOCAL_CONF: Lazy<Config> = Lazy::new(|| {
    let conf: Config = Config::builder()
        .add_source(File::with_name("config.toml"))
        .build()
        .unwrap();
    conf
});
// TODO LP: add RPC api key (and RPC endpoint, etc) to config file and JsonRpcClient
static JSON_RPC_CLIENT: Lazy<near_jsonrpc_client::JsonRpcClient> = Lazy::new(|| {
    let network_name: String = LOCAL_CONF.get("network").unwrap();
    let rpc_config = RPCConfig::default();
    let network_config = rpc_config.networks.get(&network_name).unwrap();
    let json_rpc_client = network_config.json_rpc_client();
    json_rpc_client
});
static RELAYER_ACCOUNT_ID: Lazy<String> = Lazy::new(|| {
    let relayer_account_id: String = LOCAL_CONF.get("relayer_account_id").unwrap();
    relayer_account_id
});
static KEYS_FILENAME: Lazy<String> = Lazy::new(|| {
   let keys_filename: String = LOCAL_CONF.get("keys_filename").unwrap();
    keys_filename
});
static PORT: Lazy<u16> = Lazy::new(|| {
    let port: u16 = LOCAL_CONF.get("port").unwrap();
    port
});


#[tokio::main]
async fn main() {
    // initialize tracing (aka logging)
    tracing_subscriber::registry().with(tracing_subscriber::fmt::layer()).init();

    // build our application with a route
    let app = Router::new()
        // `POST /relay` goes to `relay` handler function
        .route("/relay", post(relay));
        // See https://docs.rs/tower-http/0.1.1/tower_http/trace/index.html for more details.
        //.layer(TraceLayer::new_for_http()); // TODO LP: re-add when tower-http dependency conflict is resolved

    // run our app with hyper
    // `axum::Server` is a re-export of `hyper::Server`
    let addr = SocketAddr::from(([127, 0, 0, 1], PORT.clone()));
    info!("listening on {}", addr);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

async fn relay(
    data: extract::Json<Vec<u8>>,
) -> impl IntoResponse {
    // deserialize SignedDelegateAction using borsh
    match SignedDelegateAction::try_from_slice(&data.0) {
        Ok(signed_delegate_action) => {
            debug!("Deserialized SignedDelegateAction object: {:#?}", signed_delegate_action);

            // get the latest block hash
            let latest_final_block_hash = JSON_RPC_CLIENT
                .call(near_jsonrpc_client::methods::block::RpcBlockRequest{
                    block_reference: BlockReference::Finality(Finality::Final)
                }).await.unwrap().header.hash;  // TODO LP: better err handling on unwrap

            // create Transaction from SignedDelegateAction
            let unsigned_transaction = Transaction{
                signer_id: RELAYER_ACCOUNT_ID.as_str().parse().unwrap(),
                public_key: signed_delegate_action.delegate_action.public_key,
                nonce: signed_delegate_action.delegate_action.nonce,
                // the receiver of the txn is the sender of the signed delegate action
                receiver_id: signed_delegate_action.delegate_action.sender_id,
                block_hash: latest_final_block_hash,
                actions: signed_delegate_action.delegate_action.actions
                    .into_iter()
                    .map(|a| Action::try_from(a.clone()).unwrap())
                    .collect()
            };

            // sign transaction with locally stored key from json file
            let signed_transaction = sign_transaction(
                unsigned_transaction,
                KEYS_FILENAME.as_str(),
                JSON_RPC_CLIENT.clone(),
            ).await.unwrap().unwrap();  // TODO LP: better err handling on unwrap

            // create json_rpc_client, send the SignedTransaction
            info!("Sending transaction ...");
            let transaction_info = loop {
                let transaction_info_result = JSON_RPC_CLIENT
                    .call(RpcBroadcastTxCommitRequest{signed_transaction: signed_transaction.clone()})
                    .await;
                match transaction_info_result {
                    Ok(response) => {
                        break response;
                    }
                    Err(err) => match rpc_transaction_error(err) {
                        Ok(_) => {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await
                        }
                        Err(report) => {
                            let err_msg = String::from(
                                format!("{}: {:?}",
                                        "Error sending transaction to RPC".to_string(),
                                        report.to_string()
                                )
                            );
                            info!("{}", err_msg);
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                err_msg,
                                ).into_response()
                        },
                    },
                };
            };

            // build response json
            let mut success_msg_json: Map<String, Value> = Map::new();
            success_msg_json.insert("message".to_string(),
                                    json!("Successfully relayed and sent transaction."));
            success_msg_json.insert("Status".to_string(),
                                    json!(transaction_info.status));
            success_msg_json.insert("Transaction Outcome Logs".to_string(),
                                    json!(transaction_info.transaction_outcome.outcome.logs.join("\n")));
            info!("Success message: {:?}", success_msg_json);
            let success_msg_str = serde_json::to_string(&success_msg_json).unwrap();
            success_msg_str.into_response()
        },
        Err(e) => {
            let err_msg = String::from(
                format!("{}: {:?}",
                    "Error deserializing payload data object".to_string(),
                    e.to_string()
                )
            );
            info!("{}", err_msg);
            (
                StatusCode::BAD_REQUEST,
                err_msg,
            ).into_response()
        },
    }
}

/**
--------------------------- Testing below here ---------------------------
*/
fn create_signed_delegate_action(
    sender_id: String,
    receiver_id: String,
    actions: Vec<Action>,
    nonce: i32,
    max_block_height: i32
) -> SignedDelegateAction {
    let max_block_height: i32 = max_block_height;
    let public_key: PublicKey = PublicKey::empty(KeyType::ED25519);
    let signature: Signature = Signature::empty(KeyType::ED25519);
    SignedDelegateAction {
        delegate_action: DelegateAction {
            sender_id: sender_id.parse().unwrap(),
            receiver_id: receiver_id.parse().unwrap(),
            actions: actions
                .iter()
                .map(|a| NonDelegateAction::try_from(a.clone()).unwrap())
                .collect(),
            nonce: nonce as Nonce,
            max_block_height: max_block_height as BlockHeight,
            public_key,
        },
        signature,
    }
}

#[tokio::test]
async fn test_relay() {   // tests assume testnet in config
    // Test Transfer Action and a CreateAccount Action
    let actions = vec![
        Action::CreateAccount(CreateAccountAction {}),
        Action::Transfer(TransferAction { deposit: 1 })
    ];
    let sender_id: String = String::from("nomnomnom.testnet");
    let receiver_id: String = String::from("nomnomnom.testnet");
    let nonce: i32 = 1;
    let max_block_height = 2000000000;

    // Call the `relay` function happy path
    let signed_delegate_action = create_signed_delegate_action(
        sender_id.clone(),
        receiver_id.clone(),
        actions.clone(),
        nonce,
        max_block_height,
    );
    let json_payload = signed_delegate_action.try_to_vec().unwrap();
    println!("SignedDelegateAction Json Serialized: {:?}", json_payload);
    let response = relay(Json(Vec::from(json_payload))).await.into_response();
    let response_status = response.status();
    assert_eq!(response_status, StatusCode::OK);

    // Call the `relay` function with a payload that can't be deserialized into a SignedDelegateAction
    let bad_json_payload = serde_json::to_string("arrrgh").unwrap();
    println!("Malformed Json Payload Serialized: {:?}", bad_json_payload);
    let err_response = relay(Json(Vec::from(bad_json_payload))).await.into_response();
    let err_response_status = err_response.status();
    assert_eq!(err_response_status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore]
// TODO refactor into separate file that calls http://127.0.0.1:3030/relay instead of relay fn directly
async fn test_relay_with_load() {   // tests assume testnet in config
    // Test Transfer Action and a CreateAccount Action

    let actions = vec![
        Action::CreateAccount(CreateAccountAction {}),
        Action::Transfer(TransferAction { deposit: 1 })
    ];
    // TODO update account ids
    let account_id0: String = "nomnomnom.testnet".to_string();
    let account_id1: String = "nomnomnom.testnet".to_string();
    let mut sender_id: String = String::new();
    let mut receiver_id: String = String::new();
    let mut nonce: i32 = 1;
    let max_block_height = 2000000000;

    let num_tests = 100;
    let mut response_statuses = vec![];

    // fire off all post requests in rapid succession and save the response status codes
    for i in 0..num_tests {
        if i % 2 == 0 {
            sender_id.push_str(&*account_id0.clone());
            receiver_id.push_str(&*account_id1.clone());
        } else {
            sender_id.push_str(&*account_id1.clone());
            receiver_id.push_str(&*account_id0.clone());
        }
        // Call the `relay` function happy path
        let signed_delegate_action = create_signed_delegate_action(
            sender_id.clone(),
            receiver_id.clone(),
            actions.clone(),
            nonce,
            max_block_height,
        );
        let json_payload = signed_delegate_action.try_to_vec().unwrap();
        let response = relay(Json(Vec::from(json_payload))).await.into_response();
        response_statuses.push(response.status());

        // increment nonce & reset sender, receiver strs
        nonce += 1;
        sender_id.clear();
        receiver_id.clear();
    }

    // all responses should be successful
    for i in 0..response_statuses.len() {
        assert_eq!(response_statuses[i], StatusCode::OK);
    }
}
