use anyhow::anyhow;
use log::{debug, error, info, warn};
use reqwest::Method;
use rspotify::clients::BaseClient;
use rspotify::AuthCodePkceSpotify;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{
  sync::OnceLock,
  time::{Duration, Instant},
};
use tokio::sync::Mutex;

static SPOTIFY_API_PACING: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
const SPOTIFY_API_MIN_INTERVAL: Duration = Duration::from_millis(250);

pub async fn pace_spotify_api_call() {
  let pacing_lock = SPOTIFY_API_PACING.get_or_init(|| Mutex::new(None));
  let mut last_request_started_at = pacing_lock.lock().await;

  if let Some(last) = *last_request_started_at {
    let elapsed = last.elapsed();
    if elapsed < SPOTIFY_API_MIN_INTERVAL {
      tokio::time::sleep(SPOTIFY_API_MIN_INTERVAL - elapsed).await;
    }
  }

  *last_request_started_at = Some(Instant::now());
}

pub async fn spotify_api_request_json_for(
  spotify: &AuthCodePkceSpotify,
  method: Method,
  path: &str,
  query: &[(&str, String)],
  body: Option<Value>,
) -> anyhow::Result<Value> {
  let mut url = reqwest::Url::parse("https://api.spotify.com/v1/")?.join(path)?;
  if !query.is_empty() {
    let mut qp = url.query_pairs_mut();
    for (k, v) in query {
      qp.append_pair(k, v);
    }
  }
  debug!(
    "Spotify API request prepared: {} {} query={:?}",
    method, url, query
  );

  let started_at = Instant::now();

  let client = reqwest::Client::new();
  let mut attempt: u8 = 0;
  let max_attempts: u8 = 4;
  let mut refreshed_after_unauthorized = false;

  loop {
    let access_token = {
      let token_lock = spotify.token.lock().await.expect("Failed to lock token");
      token_lock
        .as_ref()
        .map(|t| t.access_token.clone())
        .ok_or_else(|| anyhow!("No access token available"))?
    };

    pace_spotify_api_call().await;

    let mut request = client
      .request(method.clone(), url.clone())
      .header("Authorization", format!("Bearer {}", access_token))
      .header("Content-Type", "application/json");

    if let Some(payload) = body.clone() {
      request = request.json(&payload);
    }

    debug!(
      "Spotify API request sending (attempt {}): {} {}",
      attempt + 1,
      method,
      url
    );
    let response = match request.send().await {
      Ok(response) => response,
      Err(e) => {
        error!("Spotify API send error for {} {}: {}", method, url, e);
        if attempt + 1 < max_attempts && (e.is_connect() || e.is_timeout() || e.is_request()) {
          let backoff_secs = 1 + u64::from(attempt);
          warn!(
            "Transient network error, retrying after {}s (attempt {})",
            backoff_secs,
            attempt + 1
          );
          tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
          attempt += 1;
          continue;
        }
        return Err(anyhow!("Spotify API request failed: {}", e));
      }
    };
    let status = response.status();
    if status.is_success() {
      let response_body = response.text().await?;
      let dur = started_at.elapsed();
      debug!(
        "Spotify API success {} {} status={} duration_ms={}",
        method,
        url,
        status,
        dur.as_millis()
      );
      if response_body.trim().is_empty() {
        return Ok(Value::Null);
      }
      return Ok(serde_json::from_str(&response_body)?);
    }


    if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed_after_unauthorized {
      warn!(
        "Spotify API returned 401 Unauthorized for {} {} - attempting token refresh",
        method, url
      );
      match spotify.refresh_token().await {
        Ok(_) => {
          refreshed_after_unauthorized = true;
          info!("Spotify refresh_token succeeded, retrying request");
          continue;
        }
        Err(refresh_err) => {
          let body = response.text().await.unwrap_or_default();
          error!(
            "Token refresh failed after 401 for {} {}: {}",
            method, url, refresh_err
          );
          return Err(anyhow!(
            "Spotify API {} failed: {} (token refresh failed: {})",
            status,
            body,
            refresh_err
          ));
        }
      }
    }

    if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt + 1 < max_attempts {
      let retry_after_secs = response
        .headers()
        .get("retry-after")
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1);

      let backoff_secs = retry_after_secs.max(1) + u64::from(attempt);
      warn!(
        "Rate limited by Spotify (429). retry-after={}s; backing off {}s (attempt {})",
        retry_after_secs,
        backoff_secs,
        attempt + 1
      );
      tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
      attempt += 1;
      continue;
    }

    let body = response.text().await.unwrap_or_default();
    error!(
      "Spotify API request failed: {} {} status={} body={}",
      method, url, status, body
    );
    return Err(anyhow!("Spotify API {} failed: {}", status, body));
  }
}

pub fn normalize_spotify_payload(value: &mut Value) {
  match value {
    Value::Object(map) => {
      if let Some(Value::Array(items)) = map.get_mut("items") {
        items.retain(|item| !item.is_null());
      }

      if map.contains_key("snapshot_id")
        && map.contains_key("owner")
        && map.contains_key("id")
        && !map.contains_key("tracks")
      {
        if let Some(items_obj) = map.get("items").cloned() {
          map.insert("tracks".to_string(), items_obj);
        } else {
          map.insert("tracks".to_string(), json!({ "href": "", "total": 0 }));
        }
      }

      if map.contains_key("added_at") && !map.contains_key("track") {
        if let Some(item_obj) = map.get("item").cloned() {
          map.insert("track".to_string(), item_obj);
        }
      }

      if map.contains_key("album")
        && map.contains_key("artists")
        && map.contains_key("track_number")
        && map.contains_key("duration_ms")
      {
        map
          .entry("available_markets".to_string())
          .or_insert_with(|| json!([]));
        map
          .entry("external_ids".to_string())
          .or_insert_with(|| json!({}));
        map.entry("linked_from".to_string()).or_insert(Value::Null);
        map
          .entry("popularity".to_string())
          .or_insert_with(|| json!(0));
      }

      if map.contains_key("media_type")
        && map.contains_key("languages")
        && map.contains_key("description")
        && map.contains_key("name")
      {
        map
          .entry("available_markets".to_string())
          .or_insert_with(|| json!([]));
        map
          .entry("publisher".to_string())
          .or_insert_with(|| json!(""));
      }

      if map.contains_key("album_type")
        && map.contains_key("artists")
        && map.contains_key("images")
        && map.contains_key("name")
      {
        if map.contains_key("tracks") {
          map
            .entry("available_markets".to_string())
            .or_insert(Value::Null);
          map
            .entry("external_ids".to_string())
            .or_insert_with(|| json!({}));
          map
            .entry("popularity".to_string())
            .or_insert_with(|| json!(0));
          map.entry("label".to_string()).or_insert(Value::Null);
        } else {
          map
            .entry("available_markets".to_string())
            .or_insert_with(|| json!([]));
        }
      }

      let looks_like_artist = map
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t == "artist")
        || (map.contains_key("external_urls")
          && map.contains_key("name")
          && map.contains_key("id")
          && (map.contains_key("genres") || map.contains_key("images")));

      if looks_like_artist {
        map.entry("href".to_string()).or_insert_with(|| json!(""));
        map.entry("genres".to_string()).or_insert_with(|| json!([]));
        map.entry("images".to_string()).or_insert_with(|| json!([]));
        map
          .entry("followers".to_string())
          .or_insert_with(|| json!({ "href": null, "total": 0 }));
        map
          .entry("popularity".to_string())
          .or_insert_with(|| json!(0));
      }

      for child in map.values_mut() {
        normalize_spotify_payload(child);
      }
    }
    Value::Array(values) => {
      values.retain(|item| !item.is_null());
      for child in values.iter_mut() {
        normalize_spotify_payload(child);
      }
    }
    _ => {}
  }
}

pub fn is_rate_limited_error(e: &anyhow::Error) -> bool {
  let text = e.to_string();
  text.contains("429") || text.contains("Too Many Requests") || text.contains("Too many requests")
}

#[allow(dead_code)]
pub fn is_transient_network_error(e: &anyhow::Error) -> bool {
  let text = e.to_string().to_lowercase();
  text.contains("error sending request for url")
    || text.contains("connection reset")
    || text.contains("connection refused")
    || text.contains("timed out")
    || text.contains("temporary failure")
    || text.contains("dns")
}

pub async fn spotify_get_typed_compat_for<T: DeserializeOwned>(
  spotify: &AuthCodePkceSpotify,
  path: &str,
  query: &[(&str, String)],
) -> anyhow::Result<T> {
  let mut value = spotify_api_request_json_for(spotify, Method::GET, path, query, None).await?;
  normalize_spotify_payload(&mut value);
  Ok(serde_json::from_value(value)?)
}
