//! GitHub device authorization and Copilot token exchange (public github.com).
use serde_json::Value;
use std::time::Duration;
const CLIENT_ID:&str="Iv1.b507a08c87ecfe98";
const USER_AGENT:&str="GitHubCopilotChat/0.35.0";

pub struct DeviceCode {
    pub secret:String,
    pub user_code:String,
    pub verification_uri:String,
    pub interval:Duration,
    pub expires_in:Duration,
}
pub enum Poll { Pending, SlowDown(Duration), Complete(String) }
fn client()->Result<reqwest::Client,String> {
    reqwest::Client::builder().timeout(Duration::from_secs(15)).redirect(reqwest::redirect::Policy::none()).build().map_err(|_|"Unable to create OAuth client".into())
}
pub async fn start()->Result<DeviceCode,String> { start_at("https://github.com/login/device/code").await }
async fn start_at(url:&str)->Result<DeviceCode,String> {
    let response=client()?.post(url).header("accept","application/json").header("user-agent",USER_AGENT).form(&[("client_id",CLIENT_ID),("scope","read:user")]).send().await.map_err(|_|"Device authorization request failed")?;
    if !response.status().is_success(){return Err("Device authorization rejected".into());}
    let value:Value=response.json().await.map_err(|_|"Invalid device authorization response")?;
    parse_device(&value)
}
fn parse_device(value:&Value)->Result<DeviceCode,String> {
    let text=|field|value[field].as_str().filter(|v|!v.is_empty()).map(str::to_owned).ok_or_else(||format!("Device response missing {field}"));
    let uri=text("verification_uri")?;
    let url=url::Url::parse(&uri).map_err(|_|"Invalid verification URI")?;
    if !matches!(url.scheme(),"https"|"http") || url.host_str().is_none(){return Err("Invalid verification URI".into());}
    let expires=value["expires_in"].as_u64().filter(|v|*v>0&&*v<=86400).ok_or("Invalid device expiry")?;
    Ok(DeviceCode{secret:text("device_code")?,user_code:text("user_code")?,verification_uri:uri,interval:Duration::from_secs(value["interval"].as_u64().unwrap_or(5).clamp(1,3600)),expires_in:Duration::from_secs(expires)})
}
pub async fn poll(secret:&str,interval:Duration)->Result<Poll,String>{poll_at("https://github.com/login/oauth/access_token",secret,interval).await}
async fn poll_at(url:&str,secret:&str,interval:Duration)->Result<Poll,String>{
    let response=client()?.post(url).header("accept","application/json").header("user-agent",USER_AGENT).form(&[("client_id",CLIENT_ID),("device_code",secret),("grant_type","urn:ietf:params:oauth:grant-type:device_code")]).send().await.map_err(|_|"Device polling failed")?;
    if !response.status().is_success(){return Err("Device polling rejected".into());}
    let value:Value=response.json().await.map_err(|_|"Invalid device polling response")?;
    parse_poll(&value,interval)
}
fn parse_poll(value:&Value,interval:Duration)->Result<Poll,String>{
    if let Some(token)=value["access_token"].as_str().filter(|v|!v.is_empty()){return Ok(Poll::Complete(token.into()));}
    match value["error"].as_str(){
        Some("authorization_pending")=>Ok(Poll::Pending),
        Some("slow_down")=>Ok(Poll::SlowDown(Duration::from_secs(value["interval"].as_u64().filter(|v|*v>0).unwrap_or(interval.as_secs().saturating_add(5)).clamp(1,3600)))),
        Some("access_denied")=>Err("Device authorization denied".into()),
        Some("expired_token")=>Err("Device authorization expired".into()),
        _=>Err("Invalid device polling response".into()),
    }
}
pub async fn exchange(github_token:&str)->Result<(String,i64),String>{exchange_at("https://api.github.com/copilot_internal/v2/token",github_token).await}
async fn exchange_at(url:&str,token:&str)->Result<(String,i64),String>{
    let response=client()?.get(url).bearer_auth(token).header("accept","application/json").header("user-agent",USER_AGENT).header("editor-version","vscode/1.107.0").header("editor-plugin-version","copilot-chat/0.35.0").header("copilot-integration-id","vscode-chat").send().await.map_err(|_|"Copilot token request failed")?;
    if !response.status().is_success(){return Err("Copilot subscription token rejected".into());}
    let value:Value=response.json().await.map_err(|_|"Invalid Copilot token response")?;
    let token=value["token"].as_str().filter(|v|!v.is_empty()).ok_or("Missing Copilot access token")?;
    let expires=value["expires_at"].as_i64().and_then(|v|v.checked_mul(1000)).filter(|v|*v>chrono::Utc::now().timestamp_millis()).ok_or("Invalid Copilot token expiry")?;
    Ok((token.into(),expires))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::{get,post},Json,Router,http::HeaderMap,body::Bytes};
    #[test]
    fn pending_slow_down_denial_and_untrusted_verification_are_distinct() {
        assert!(matches!(parse_poll(&serde_json::json!({"error":"authorization_pending"}),Duration::from_secs(5)).unwrap(),Poll::Pending));
        assert!(matches!(parse_poll(&serde_json::json!({"error":"slow_down"}),Duration::from_secs(5)).unwrap(),Poll::SlowDown(d) if d==Duration::from_secs(10)));
        assert!(matches!(parse_poll(&serde_json::json!({"error":"slow_down","interval":17}),Duration::from_secs(5)).unwrap(),Poll::SlowDown(d) if d==Duration::from_secs(17)));
        assert!(parse_poll(&serde_json::json!({"error":"access_denied"}),Duration::from_secs(5)).is_err());
        assert!(parse_device(&serde_json::json!({"device_code":"secret","user_code":"ABCD","verification_uri":"file:///private","expires_in":600})).is_err());
    }
    #[tokio::test]
    async fn device_and_subscription_exchange_use_reference_wire_contracts() {
        let app=Router::new()
            .route("/device",post(|headers:HeaderMap,body:Bytes|async move{
                assert_eq!(headers["accept"],"application/json");
                let fields:std::collections::HashMap<_,_>=url::form_urlencoded::parse(&body).into_owned().collect();
                assert_eq!(fields["client_id"],CLIENT_ID);assert_eq!(fields["scope"],"read:user");
                Json(serde_json::json!({"device_code":"private-device-code","user_code":"ABCD","verification_uri":"https://github.com/login/device","expires_in":600}))
            }))
            .route("/poll",post(|body:Bytes|async move{
                let fields:std::collections::HashMap<_,_>=url::form_urlencoded::parse(&body).into_owned().collect();
                assert_eq!(fields["device_code"],"private-device-code");assert_eq!(fields["grant_type"],"urn:ietf:params:oauth:grant-type:device_code");
                Json(serde_json::json!({"access_token":"github-token"}))
            }))
            .route("/subscription",get(|headers:HeaderMap|async move{
                assert_eq!(headers["authorization"],"Bearer github-token");assert_eq!(headers["editor-version"],"vscode/1.107.0");
                Json(serde_json::json!({"token":"copilot-token","expires_at":chrono::Utc::now().timestamp()+3600}))
            }));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();
        let server=tokio::spawn(async move {axum::serve(listener,app).await.unwrap()});
        let device=start_at(&format!("http://{addr}/device")).await.unwrap();assert_eq!(device.interval,Duration::from_secs(5));
        let Poll::Complete(token)=poll_at(&format!("http://{addr}/poll"),&device.secret,device.interval).await.unwrap() else {panic!("expected token")};
        let (access,expiry)=exchange_at(&format!("http://{addr}/subscription"),&token).await.unwrap();
        assert_eq!(access,"copilot-token");assert!(expiry>chrono::Utc::now().timestamp_millis());server.abort();
    }
}
