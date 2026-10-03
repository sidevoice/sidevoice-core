//! Symmetric JSON-RPC 2.0 on the local Unix listener.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::control::room::{ConnectorPeer, PeerRequest};
use super::AppState;

const MAX_FRAME: usize = 1024 * 1024;
fn id_valid(id: &Value) -> bool { id.as_str().is_some_and(|s|!s.is_empty()&&s.len()<=100) || id.as_i64().is_some_and(|n|n.abs()<=9_007_199_254_740_991) }
fn decode(text: &str) -> Result<Value,u16> {if text.len()>MAX_FRAME{return Err(1009)}let v:Value=serde_json::from_str(text).map_err(|_|1002u16)?;
    if !v.is_object()||v["jsonrpc"]!="2.0"{return Err(1002)}
    if let Some(method)=v.get("method") {if method.as_str().is_none_or(|s|s.is_empty()||s.len()>128)||v.get("result").is_some()||v.get("error").is_some()||v.get("params").is_some_and(|p|!p.is_object())||v.get("id").is_some_and(|i|!id_valid(i)){return Err(1002)}}
    else if v.get("id").is_none_or(|i|!id_valid(i))||v.get("result").is_some()==v.get("error").is_some(){return Err(1002)}
    Ok(v)}
fn error(id: Value,code:i64,key:&str)->Value{json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":key}})}
fn text(v:Value)->Option<Message>{let s=serde_json::to_string(&v).ok()?;(s.len()<=MAX_FRAME).then(||Message::Text(s.into()))}
fn close(code:u16)->Message{Message::Close(Some(CloseFrame{code,reason:"".into()}))}
fn field<'a>(v:&'a Value,k:&str)->&'a str{v.get(k).and_then(Value::as_str).unwrap_or("")}

async fn dispatch(state:Arc<AppState>,cid:String,method:String,params:Value)->Result<Value,(i64,&'static str)>{
    let room=&state.room;
    match method.as_str(){
        "binding.register"=>Ok(room.register(&cid,&params).unwrap_or_else(|e|json!({"error":e.key}))),
        "binding.unregister"=>{room.unregister(&cid,field(&params,"binding_id"));Ok(Value::Null)},
        "speech.publish"=>{if ["event_id","utterance_id"].iter().any(|key|{let s=field(&params,key);s.is_empty()||s.len()>200}){return Err((-32602,"speech.publish requires bounded identifiers"))}
            let mut result=room.connector_speech(&cid,&params,true);
            if let Some(obj)=result.as_object_mut(){obj.insert("event_id".into(),params.get("event_id").cloned().unwrap_or(Value::Null));obj.insert("utterance_id".into(),params.get("utterance_id").cloned().unwrap_or(Value::Null));}Ok(result)},
        "input.working"=>{room.working(&cid,&params);Ok(Value::Null)},
        "input.engine"=>{room.engine(&cid,&params);Ok(Value::Null)},
        "input.read"=>{room.read(&cid,&params);Ok(Value::Null)},
        "device.pairing_code"=>Ok(state.issue_code()),
        _=>Err((-32601,"Method not found")),
    }
}

pub async fn run(state:Arc<AppState>,mut socket:WebSocket){
    let first=tokio::time::timeout(Duration::from_secs(10),socket.recv()).await;
    let Some(Ok(Message::Text(raw)))=first.ok().flatten() else{let _=socket.send(close(1008)).await;return;};
    let Ok(hello)=decode(&raw) else{let _=socket.send(close(1008)).await;return;};
    let params=hello.get("params").cloned().unwrap_or(json!({}));let rid=hello.get("id").cloned();
    if hello["method"]!="connector.hello"||rid.is_none() {let _=socket.send(close(1008)).await;return;}
    let rid=rid.unwrap();let cid=field(&params,"connector_id");let token=field(&params,"token");
    if params["protocol"]!=3||cid.is_empty()||cid.len()>200||token.is_empty()||token.len()>512{
        if let Some(msg)=text(error(rid,-32002,"Protocol 3 hello is required")){let _=socket.send(msg).await;}let _=socket.send(close(1008)).await;return;}
    if !state.room.authenticate_connector(cid,token,&params){if let Some(msg)=text(error(rid,-32001,"Connector credential refused")){let _=socket.send(msg).await;}let _=socket.send(close(1008)).await;return;}
    let cid=cid.to_owned();let generation=Uuid::new_v4().to_string();
    let (requests,mut rx)=mpsc::channel::<PeerRequest>(128);let peer=ConnectorPeer{generation:generation.clone(),sender:requests};
    let old=state.room.attach(&cid,peer);
    if let Some(old)=old{let _=old.send("connector.replaced",json!({})).await;}
    let (out,mut outgoing)=mpsc::channel::<Value>(128);
    let _=out.send(json!({"jsonrpc":"2.0","id":rid,"result":{"protocol":3}})).await;
    let _=out.send(json!({"jsonrpc":"2.0","method":"connector.welcome","params":{"protocol":3}})).await;
    let mut pending:HashMap<String,oneshot::Sender<Result<Value,()>>>=HashMap::new();
    let mut incoming=HashSet::<String>::new();let mut seq=0u64;
    loop{tokio::select!{
        command=rx.recv()=>{let Some(command)=command else{break};if let Some(answer)=command.answer{
            if pending.len()>=128{let _=answer.send(Err(()));continue;}seq+=1;let request_id=format!("s:{seq}");
            pending.insert(request_id.clone(),answer);if out.send(json!({"jsonrpc":"2.0","id":request_id,"method":command.method,"params":command.params})).await.is_err(){break;}
        }else if out.send(json!({"jsonrpc":"2.0","method":command.method,"params":command.params})).await.is_err(){break;}},
        frame=outgoing.recv()=>{let Some(frame)=frame else{break};let Some(msg)=text(frame) else{let _=socket.send(close(1009)).await;break;};if socket.send(msg).await.is_err(){break;}},
        frame=socket.recv()=>{let Some(Ok(frame))=frame else{break};let Message::Text(raw)=frame else{if matches!(frame,Message::Binary(_)){let _=socket.send(close(1003)).await;break;}continue;};
            let incoming_frame=match decode(&raw){Ok(v)=>v,Err(code)=>{let _=socket.send(close(code)).await;break;}};
            if incoming_frame.get("method").is_none(){let key=incoming_frame["id"].as_str().map(str::to_owned).unwrap_or_else(||incoming_frame["id"].to_string());
                if let Some(answer)=pending.remove(&key){let outcome=if incoming_frame.get("error").is_some(){Err(())}else{Ok(incoming_frame.get("result").cloned().unwrap_or(Value::Null))};let _=answer.send(outcome);}continue;}
            let request_id=incoming_frame.get("id").cloned();let key=request_id.as_ref().map(ToString::to_string).unwrap_or_default();
            if request_id.is_some()&&!incoming.insert(key.clone()){let _=socket.send(close(1002)).await;break;}
            if incoming.len()>32{if let Some(id)=request_id{let _=out.send(error(id,-32000,"Connector request capacity reached")).await;incoming.remove(&key);}continue;}
            let method=field(&incoming_frame,"method").to_owned();let params=incoming_frame.get("params").cloned().unwrap_or(json!({}));let out=out.clone();let state=state.clone();let cid=cid.clone();
            tokio::spawn(async move{let outcome=tokio::time::timeout(Duration::from_secs(60),dispatch(state,cid,method,params)).await;
                if let Some(id)=request_id{let frame=match outcome{Ok(Ok(result))=>json!({"jsonrpc":"2.0","id":id,"result":result}),Ok(Err((code,msg)))=>error(id,code,msg),Err(_)=>error(id,-32001,"Request timed out")};let _=out.send(frame).await;}});
            incoming.remove(&key);
        }
    }}
    state.room.detach(&cid,&generation);
    for (_,answer) in pending {let _=answer.send(Err(()));}
}
