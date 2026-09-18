//! Kimi Code RFC 8628 device grant.
use crate::copilot_oauth::DeviceCode;
use serde_json::Value;
use std::time::Duration;
const CLIENT_ID:&str="17e5f671-d194-4dfb-9706-5516cb48c098";
pub enum Poll { Pending, SlowDown(Duration), Complete{access:String,refresh:String,expires:i64} }
fn client()->Result<reqwest::Client,String>{reqwest::Client::builder().timeout(Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build().map_err(|_|"Unable to initialize OAuth client".into())}
pub async fn start()->Result<DeviceCode,String>{start_at("https://auth.kimi.com/api/oauth/device_authorization").await}
async fn start_at(url:&str)->Result<DeviceCode,String>{
    let response=client()?.post(url).header("accept","application/json").form(&[("client_id",CLIENT_ID)]).send().await.map_err(|_|"Kimi device request failed")?;
    if !response.status().is_success(){return Err("Kimi device authorization rejected".into());}
    let value:Value=response.json().await.map_err(|_|"Invalid Kimi device response")?;
    parse_device(&value)
}
fn parse_device(value:&Value)->Result<DeviceCode,String>{
    let field=|key|value[key].as_str().filter(|v|!v.is_empty()).map(str::to_owned).ok_or_else(||format!("Missing Kimi device field {key}"));
    for key in ["verification_uri","verification_uri_complete"] {
        let url=url::Url::parse(&field(key)?).map_err(|_|"Invalid Kimi verification URL")?;
        if !matches!(url.scheme(),"https"|"http")||url.host_str().is_none(){return Err("Invalid Kimi verification URL".into());}
    }
    Ok(DeviceCode{secret:field("device_code")?,user_code:field("user_code")?,verification_uri:field("verification_uri_complete")?,interval:Duration::from_secs(value["interval"].as_u64().filter(|v|*v>0).unwrap_or(5).min(3600)),expires_in:Duration::from_secs(value["expires_in"].as_u64().filter(|v|*v>0).unwrap_or(900).min(86400))})
}
pub async fn refresh(refresh_token:&str)->Result<(String,String,i64),String>{refresh_at("https://auth.kimi.com/api/oauth/token",refresh_token).await}
async fn refresh_at(url:&str,refresh_token:&str)->Result<(String,String,i64),String>{
    let mut last=None;
    for attempt in 0..=3 {
        if attempt>0 {tokio::time::sleep(Duration::from_secs(1<<(attempt-1))).await;}
        let response=match client()?.post(url).header("accept","application/json").form(&[("client_id",CLIENT_ID),("grant_type","refresh_token"),("refresh_token",refresh_token)]).send().await {Ok(response)=>response,Err(_)=>{last=Some("Kimi token refresh failed".to_owned());continue;}};
        let status=response.status();let value:Value=response.json().await.map_err(|_|"Invalid Kimi refresh response")?;
        if status.is_success() {return parse_refresh(&value);}
        if status==reqwest::StatusCode::UNAUTHORIZED||status==reqwest::StatusCode::FORBIDDEN||value["error"]=="invalid_grant" {return Err("Kimi token refresh unauthorized".into());}
        if !(status==reqwest::StatusCode::TOO_MANY_REQUESTS||status.is_server_error())||attempt==3{return Err("Kimi token refresh rejected".into());}
        last=Some("Kimi token refresh failed".into());
    }
    Err(last.unwrap_or_else(||"Kimi token refresh failed".into()))
}
fn parse_refresh(value:&Value)->Result<(String,String,i64),String>{
    let access=value["access_token"].as_str().filter(|v|!v.is_empty()).ok_or("Missing Kimi access token")?;
    let refresh=value["refresh_token"].as_str().filter(|v|!v.is_empty()).ok_or("Missing Kimi refresh token")?;
    let expires=value["expires_in"].as_i64().filter(|v|*v>0).and_then(|v|v.checked_mul(1000)).and_then(|v|chrono::Utc::now().timestamp_millis().checked_add(v)).ok_or("Invalid Kimi expiry")?;
    Ok((access.into(),refresh.into(),expires))
}

pub async fn poll(secret:&str,interval:Duration)->Result<Poll,String>{poll_at("https://auth.kimi.com/api/oauth/token",secret,interval).await}
async fn poll_at(url:&str,secret:&str,interval:Duration)->Result<Poll,String>{
    let response=client()?.post(url).header("accept","application/json").form(&[("client_id",CLIENT_ID),("device_code",secret),("grant_type","urn:ietf:params:oauth:grant-type:device_code")]).send().await.map_err(|_|"Kimi device polling failed")?;
    if response.status().is_server_error(){return Err("Kimi device upstream failed".into());}
    let success=response.status().is_success();
    let value:Value=response.json().await.map_err(|_|"Invalid Kimi token response")?;
    if success&&value["access_token"].is_string(){
        let access=value["access_token"].as_str().filter(|v|!v.is_empty()).ok_or("Missing Kimi access token")?;
        let refresh=value["refresh_token"].as_str().filter(|v|!v.is_empty()).ok_or("Missing Kimi refresh token")?;
        let lifetime=value["expires_in"].as_i64().filter(|v|*v>0).and_then(|v|v.checked_mul(1000)).ok_or("Invalid Kimi expiry")?;
        let expires=chrono::Utc::now().timestamp_millis().checked_add(lifetime).ok_or("Invalid Kimi expiry")?;
        return Ok(Poll::Complete{access:access.into(),refresh:refresh.into(),expires});
    }
    match value["error"].as_str(){
        Some("authorization_pending")=>Ok(Poll::Pending),
        Some("slow_down")=>Ok(Poll::SlowDown(Duration::from_secs(value["interval"].as_u64().filter(|v|*v>0).unwrap_or(interval.as_secs().saturating_add(5)).min(3600)))),
        Some("access_denied")=>Err("Kimi device authorization denied".into()),
        Some("expired_token")=>Err("Kimi device authorization expired".into()),
        _=>Err("Kimi device token rejected".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post,Json,Router,http::StatusCode,body::Bytes};
    #[test]
    fn verification_urls_are_validated_and_interval_defaults_match_reference() {
        let mut body=serde_json::json!({"device_code":"secret","user_code":"ABCD","verification_uri":"https://auth.kimi.com/device","verification_uri_complete":"https://auth.kimi.com/device?code=ABCD"});
        let device=parse_device(&body).unwrap();assert_eq!(device.interval,Duration::from_secs(5));assert_eq!(device.expires_in,Duration::from_secs(900));
        body["verification_uri_complete"]="javascript:bad".into();assert!(parse_device(&body).is_err());
    }
    #[tokio::test]
    async fn kimi_pending_error_status_and_complete_token_are_parsed() {
        let app=Router::new().route("/pending",post(||async{(StatusCode::BAD_REQUEST,Json(serde_json::json!({"error":"authorization_pending"})))}))
            .route("/token",post(|body:Bytes|async move{
                let fields:std::collections::HashMap<_,_>=url::form_urlencoded::parse(&body).into_owned().collect();
                assert_eq!(fields["client_id"],CLIENT_ID);assert_eq!(fields["device_code"],"secret");
                Json(serde_json::json!({"access_token":"access","refresh_token":"refresh","expires_in":3600}))
            }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();
        let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        assert!(matches!(poll_at(&format!("http://{addr}/pending"),"secret",Duration::from_secs(5)).await.unwrap(),Poll::Pending));
        let Poll::Complete{access,refresh,expires}=poll_at(&format!("http://{addr}/token"),"secret",Duration::from_secs(5)).await.unwrap() else {panic!("expected token")};
        assert_eq!(access,"access");assert_eq!(refresh,"refresh");assert!(expires>chrono::Utc::now().timestamp_millis());server.abort();
    }
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use axum::{routing::post,Json,Router,http::StatusCode,body::Bytes};
    use std::sync::{Arc,atomic::{AtomicUsize,Ordering}};
    #[tokio::test]
    async fn refresh_rotates_token_retries_transient_and_rejects_invalid_grant() {
        let hits=Arc::new(AtomicUsize::new(0));let count=hits.clone();
        let app=Router::new().route("/refresh",post(move |body:Bytes| {let count=count.clone();async move{
            let fields:std::collections::HashMap<_,_>=url::form_urlencoded::parse(&body).into_owned().collect();assert_eq!(fields["grant_type"],"refresh_token");assert_eq!(fields["refresh_token"],"old");
            if count.fetch_add(1,Ordering::SeqCst)==0 {(StatusCode::SERVICE_UNAVAILABLE,Json(serde_json::json!({"error":"busy"})))} else {(StatusCode::OK,Json(serde_json::json!({"access_token":"new","refresh_token":"rotated","expires_in":3600})))}
        }}));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();let server=tokio::spawn(async move{axum::serve(listener,app).await.unwrap()});
        let (access,refresh,_) = refresh_at(&format!("http://{addr}/refresh"),"old").await.unwrap();assert_eq!((access,refresh),("new".into(),"rotated".into()));assert_eq!(hits.load(Ordering::SeqCst),2);server.abort();
        assert_eq!(refresh_at("http://127.0.0.1:1/nope","old").await.unwrap_err(),"Kimi token refresh failed");
    }
}
