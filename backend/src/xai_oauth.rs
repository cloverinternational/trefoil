//! xAI OAuth device-code grant with strict HTTPS verification URLs.
use crate::copilot_oauth::DeviceCode;
use serde_json::Value;
use std::time::Duration;
const CLIENT_ID:&str="b1a00492-073a-47ea-816f-4c329264a828";
const SCOPE:&str="openid profile email offline_access grok-cli:access api:access";
pub enum Poll{Pending,SlowDown(Duration),Complete{access:String,refresh:String,expires:i64}}
fn client()->Result<reqwest::Client,String>{reqwest::Client::builder().timeout(Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build().map_err(|_|"Unable to initialize xAI OAuth client".into())}
fn verification(value:&str)->Result<String,String>{let url=url::Url::parse(value).map_err(|_|"Untrusted xAI verification URI")?;if url.scheme()!="https"||url.host_str().is_none(){return Err("Untrusted xAI verification URI".into())}Ok(url.into())}
fn tokens(value:&Value,old:Option<&str>)->Result<(String,String,i64),String>{
 let access=value["access_token"].as_str().filter(|v|!v.is_empty()).ok_or("Invalid xAI OAuth access token")?;
 let refresh=value["refresh_token"].as_str().filter(|v|!v.is_empty()).map(str::to_owned).or_else(||old.map(str::to_owned)).ok_or("Invalid xAI OAuth refresh token")?;
 let seconds=value["expires_in"].as_i64().unwrap_or(3600);if seconds<=0{return Err("Invalid xAI OAuth expiry".into())};
 Ok((access.into(),refresh,chrono::Utc::now().timestamp_millis().saturating_add(seconds.saturating_mul(1000)).saturating_sub(300_000)))
}
pub async fn start()->Result<DeviceCode,String>{start_at("https://auth.x.ai/oauth2/device/code").await}
async fn start_at(url:&str)->Result<DeviceCode,String>{let response=client()?.post(url).header("accept","application/json").form(&[("client_id",CLIENT_ID),("scope",SCOPE),("referrer","pi")]).send().await.map_err(|_|"xAI device request failed")?;if !response.status().is_success(){return Err("xAI device authorization rejected".into())};let value:Value=response.json().await.map_err(|_|"Invalid xAI device response")?;let field=|key|value[key].as_str().filter(|v|!v.is_empty()).map(str::to_owned).ok_or_else(||format!("Missing xAI device field {key}"));let uri=verification(&value["verification_uri_complete"].as_str().unwrap_or(value["verification_uri"].as_str().unwrap_or("")))?;Ok(DeviceCode{secret:field("device_code")?,user_code:field("user_code")?,verification_uri:uri,interval:Duration::from_secs(value["interval"].as_u64().filter(|v|*v>0).unwrap_or(5).min(3600)),expires_in:Duration::from_secs(value["expires_in"].as_u64().filter(|v|*v>0).ok_or("Invalid xAI device expiry")?.min(86400))})}
pub async fn poll(secret:&str,interval:Duration)->Result<Poll,String>{poll_at("https://auth.x.ai/oauth2/token",secret,interval).await}
async fn poll_at(url:&str,secret:&str,interval:Duration)->Result<Poll,String>{
    let response=client()?.post(url).header("accept","application/json").form(&[("grant_type","urn:ietf:params:oauth:grant-type:device_code"),("client_id",CLIENT_ID),("device_code",secret)]).send().await.map_err(|_|"xAI polling failed")?;
    let ok=response.status().is_success();
    let value:Value=response.json().await.map_err(|_|"Invalid xAI token response")?;
    if ok {let(a,r,e)=tokens(&value,None)?;return Ok(Poll::Complete{access:a,refresh:r,expires:e});}
    match value["error"].as_str(){
        Some("authorization_pending")=>Ok(Poll::Pending),
        Some("slow_down")=>{
            let seconds=value["interval"].as_u64().filter(|v|*v>0).unwrap_or(interval.as_secs().saturating_add(5)).min(3600);
            Ok(Poll::SlowDown(Duration::from_secs(seconds)))
        }
        Some("access_denied")|Some("authorization_denied")=>Err("xAI device authorization denied".into()),
        Some("expired_token")=>Err("xAI device code expired".into()),
        _=>Err("xAI device token rejected".into())
    }
}
pub async fn refresh(token:&str)->Result<(String,String,i64),String>{refresh_at("https://auth.x.ai/oauth2/token",token).await}
async fn refresh_at(url:&str,token:&str)->Result<(String,String,i64),String>{let response=client()?.post(url).header("accept","application/json").form(&[("grant_type","refresh_token"),("client_id",CLIENT_ID),("refresh_token",token)]).send().await.map_err(|_|"xAI refresh failed")?;if !response.status().is_success(){return Err("xAI refresh rejected".into())};tokens(&response.json::<Value>().await.map_err(|_|"Invalid xAI refresh response")?,Some(token))}

#[cfg(test)]
mod tests {
 use super::*;use axum::{routing::post,Json,Router,http::{StatusCode,HeaderMap},body::Bytes};
 #[test] fn rejects_non_https_and_retains_refresh_when_omitted(){assert!(verification("http://x.ai").is_err());let(a,r,_)=tokens(&serde_json::json!({"access_token":"a"}),Some("old")).unwrap();assert_eq!((a,r),("a".into(),"old".into()));}
 #[tokio::test] async fn xai_device_and_refresh_forms_follow_contract(){let app=Router::new().route("/device",post(|body:Bytes|async move{let f:std::collections::HashMap<_,_>=url::form_urlencoded::parse(&body).into_owned().collect();assert_eq!(f["scope"],SCOPE);Json(serde_json::json!({"device_code":"secret","user_code":"ABC","verification_uri":"https://auth.x.ai/device","expires_in":600}))})).route("/poll",post(|_:HeaderMap|async{(StatusCode::BAD_REQUEST,Json(serde_json::json!({"error":"slow_down","interval":11})))})).route("/refresh",post(|body:Bytes|async move{let f:std::collections::HashMap<_,_>=url::form_urlencoded::parse(&body).into_owned().collect();assert_eq!(f["grant_type"],"refresh_token");assert_eq!(f["refresh_token"],"old");Json(serde_json::json!({"access_token":"new","expires_in":3600}))}));let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});assert_eq!(start_at(&format!("http://{addr}/device")).await.unwrap().user_code,"ABC");assert!(matches!(poll_at(&format!("http://{addr}/poll"),"secret",Duration::from_secs(5)).await.unwrap(),Poll::SlowDown(d) if d==Duration::from_secs(11)));let(a,r,_)=refresh_at(&format!("http://{addr}/refresh"),"old").await.unwrap();assert_eq!((a,r),("new".into(),"old".into()));server.abort();}
}
