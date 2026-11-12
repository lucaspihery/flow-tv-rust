//! Client minimal pour l'API Xtream Codes (`player_api.php`), partie live uniquement.

use std::time::Duration;

use base64::Engine;
use serde_json::Value;

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) FlowTV/0.1";

#[derive(Debug, Clone)]
pub struct Credentials {
    pub server: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub struct AccountInfo {
    pub status: String,
    pub exp_date: Option<i64>,
    pub max_connections: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Category {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub stream_id: i64,
    pub num: i64,
    pub name: String,
    pub logo: String,
    pub category_id: String,
    pub epg_id: String,
}

#[derive(Debug, Clone)]
pub struct EpgProgram {
    pub title: String,
    pub description: String,
    pub start: i64,
    pub end: i64,
}

/// Normalise l'adresse du serveur : ajoute `http://` si besoin et retire le `/` final.
pub fn normalize_server(input: &str) -> String {
    let mut s = input.trim().trim_end_matches('/').to_string();
    if let Some(stripped) = s.strip_suffix("/player_api.php") {
        s = stripped.to_string();
    }
    if !s.starts_with("http://") && !s.starts_with("https://") {
        s = format!("http://{s}");
    }
    s
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .user_agent(USER_AGENT)
        .build()
        .into()
}

fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub struct Client {
    creds: Credentials,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(creds: Credentials) -> Self {
        Self { creds, agent: agent() }
    }

    fn api_url(&self, action: Option<&str>, extra: &str) -> String {
        let mut url = format!(
            "{}/player_api.php?username={}&password={}",
            self.creds.server,
            enc(&self.creds.username),
            enc(&self.creds.password)
        );
        if let Some(a) = action {
            url.push_str("&action=");
            url.push_str(a);
        }
        url.push_str(extra);
        url
    }

    fn get_json(&self, url: &str) -> Result<Value, String> {
        let mut resp = self.agent.get(url).call().map_err(|e| humanize(&e))?;
        let body = resp
            .body_mut()
            .with_config()
            .limit(512 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| format!("Réponse illisible : {e}"))?;
        serde_json::from_str(&body).map_err(|_| "Le serveur n'a pas renvoyé de données valides.".to_string())
    }

    pub fn authenticate(&self) -> Result<AccountInfo, String> {
        let v = self.get_json(&self.api_url(None, ""))?;
        let ui = v.get("user_info").ok_or("Réponse inattendue du serveur (pas de user_info).")?;
        if as_i64(ui.get("auth")).unwrap_or(0) != 1 {
            return Err("Identifiants refusés par le serveur.".into());
        }
        let status = as_string(ui.get("status")).unwrap_or_else(|| "Active".into());
        if !status.eq_ignore_ascii_case("active") {
            return Err(format!("Abonnement non actif (statut : {status})."));
        }
        Ok(AccountInfo {
            status,
            exp_date: as_i64(ui.get("exp_date")),
            max_connections: as_i64(ui.get("max_connections")),
        })
    }

    pub fn live_categories(&self) -> Result<Vec<Category>, String> {
        let v = self.get_json(&self.api_url(Some("get_live_categories"), ""))?;
        Ok(v.as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| {
                        Some(Category {
                            id: as_string(c.get("category_id"))?,
                            name: as_string(c.get("category_name")).unwrap_or_default().trim().to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn live_streams(&self) -> Result<Vec<Channel>, String> {
        let v = self.get_json(&self.api_url(Some("get_live_streams"), ""))?;
        let arr = v.as_array().ok_or("Liste des chaînes invalide.")?;
        let mut out = Vec::with_capacity(arr.len());
        for s in arr {
            let Some(stream_id) = as_i64(s.get("stream_id")) else { continue };
            out.push(Channel {
                stream_id,
                num: as_i64(s.get("num")).unwrap_or(0),
                name: as_string(s.get("name")).unwrap_or_default().trim().to_string(),
                logo: as_string(s.get("stream_icon")).unwrap_or_default().trim().to_string(),
                category_id: as_string(s.get("category_id")).unwrap_or_default(),
                epg_id: as_string(s.get("epg_channel_id")).unwrap_or_default(),
            });
        }
        Ok(out)
    }

    pub fn short_epg(&self, stream_id: i64, limit: u32) -> Result<Vec<EpgProgram>, String> {
        let v = self.get_json(
            &self.api_url(Some("get_short_epg"), &format!("&stream_id={stream_id}&limit={limit}")),
        )?;
        let list = v.get("epg_listings").and_then(|l| l.as_array()).cloned().unwrap_or_default();
        let mut out: Vec<EpgProgram> = list
            .iter()
            .filter_map(|e| {
                let start = as_i64(e.get("start_timestamp"))?;
                let end = as_i64(e.get("stop_timestamp")).or_else(|| as_i64(e.get("end_timestamp")))?;
                Some(EpgProgram {
                    title: b64(as_string(e.get("title")).unwrap_or_default()),
                    description: b64(as_string(e.get("description")).unwrap_or_default()),
                    start,
                    end,
                })
            })
            .collect();
        out.sort_by_key(|p| p.start);
        Ok(out)
    }
}

pub fn stream_url(c: &Credentials, stream_id: i64) -> String {
    format!("{}/live/{}/{}/{}.ts", c.server, enc(&c.username), enc(&c.password), stream_id)
}

fn humanize(e: &ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(401) | ureq::Error::StatusCode(403) => {
            "Accès refusé par le serveur (identifiants ou IP bloquée).".into()
        }
        ureq::Error::StatusCode(404) => "Adresse du serveur introuvable (404).".into(),
        ureq::Error::StatusCode(c) => format!("Le serveur a répondu avec l'erreur {c}."),
        ureq::Error::Timeout(_) => "Le serveur ne répond pas (délai dépassé).".into(),
        ureq::Error::HostNotFound => "Serveur introuvable, vérifiez l'adresse.".into(),
        ureq::Error::ConnectionFailed => "Connexion au serveur impossible.".into(),
        other => format!("Erreur réseau : {other}"),
    }
}

fn b64(s: String) -> String {
    let t = s.trim();
    if t.is_empty() {
        return String::new();
    }
    match base64::engine::general_purpose::STANDARD.decode(t) {
        Ok(bytes) => String::from_utf8(bytes).map(|x| x.trim().to_string()).unwrap_or(s),
        Err(_) => s,
    }
}

fn as_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn as_i64(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_normalization() {
        assert_eq!(normalize_server("example.com:8080/"), "http://example.com:8080");
        assert_eq!(normalize_server("https://a.b/player_api.php"), "https://a.b");
    }

    #[test]
    fn base64_titles() {
        assert_eq!(b64("SmVhbg==".into()), "Jean");
        assert_eq!(b64("pas du base64 !".into()), "pas du base64 !");
    }

    #[test]
    fn url_encoding() {
        assert_eq!(enc("a b&c"), "a%20b%26c");
    }
}
