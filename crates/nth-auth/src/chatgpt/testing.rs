//! A stand-in for auth.openai.com: answers every request with one canned
//! reply, serves the keys and configuration a login reads, and keeps what
//! was asked.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

use super::jwt::Jwks;

/// The RSA key test ID tokens are signed with; its public half is the JWK
/// below.
const PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCOb7trqJb+WxqD
Q97aN5Q93NlJCn9ucKW2jjJyZEq7vRUZG5xV17KD0BUi8OGw3F2Rx1Vhg+AnD3Iq
xtP+1SxKgzLqzx4ArkOeE2LanM4fbvZskzqEi/AQ5y0zgwbkRG0Hx7IjUDkzikwe
3egKXQoGx0ntoVVy8o7sAPfGS8katsOkP5UVeN3afyxmrliytExlcytOObFmwT5p
2MBE5s3JdNF1/O9xN6Kq6Yyk3iPsWtyacZDt6CeYw9I2Jpnj0v+4VmmwTixe74S1
QeGnGlgMwBLHQc3ZUG9OCCJeLT2hvb03gaogx7dXv4O20t56R/OUB0wydA0csWSj
ATRb/DNVAgMBAAECggEAARPDLVRsgT5HVuKZSI7/6bTKcEnOmk8MyLcWybZ7dYo3
K3cS9SsbIuCpjQ8yb+OcWnq3nEk4ADBsNxzNodWTFUalETwBbNkozjgjx7+I+HdJ
ZgHPb/vqRzxQVn+xRzeHhuTnB9Fnu72NUAXH78PcboR960WEDFpDuSzc1PqmNqtD
BPcpUocJIC7RSc+zB54e0h7biAvm/DfFgDOVIYyYrmxqrxeBfYr1HPCDQi6mlH45
SNtGfH2hrD5F+yUeI51dV/BMIbjNJVA5FlxvALOe0I1w5lQ18281E5pLzCWcj1Uk
JXWZ4cm6lwSHZUXJsdatO8Hh7aoPM0T4lHugI3UhQwKBgQDCxzF+oFqCgysC89S9
3/yK8oLUa0auB8hj5NiQlhKeCSjWQb6DbBE8ZTDq7mbLFroKdB7H5migJ64Lk72H
bvuG/eh/2Ms2okx5Uut8LK29Yw5GXX93pGFxSNhHwn1OZ9WlocoF+omXD7IST45H
DABaTarOGrv1vS++h+I0LVaNlwKBgQC7NN8tdxX2O7C34y4nUpARWWrLZF6WA+Yu
QojNxT+a4efKoWNK9J04RR9d4VlbcjKpIbZzIkLtPejPSPxfOkR1skO61FOYabf2
eUJ+t6c8p5mHqfBVddiavd7zduyakIH4u0t6zLnKSvPVmxC3XzZcmgce5IoJbfvL
nHhCi5478wKBgET3TNIeKw4NLZyVA13bwkrchXB7L4KNZOnh7MB9MshcS+ZZTwCv
sgalecWldiYwvNzYstFrBt62TgzxWelrZYotuzYRaBrHKrbGonTMSRRGz5YgovKU
9J+tAE+gUMgDQgWsnKCzTiZ7uzx6iNElMbbHy1PA6Y6gVxhqerCo4P2vAoGAcXdI
Vdw5+XaQU8Rkcvz9ySYzLnf1RGO0nhFBrcaCGLQXs59B+Wpt6G8s5c/yQxTiU9TV
Ks93VSgS9aA9JtLAL74OgyyyRky9baxtezSBLqg+zBVaT9IPqSqv+Uz5d21i+Yk7
5IA1k8D+LVfBOAODhXjuG/FVIQYCeZSq51qMW7UCgYA66oY2EO7lu3kWh4t3KaFt
WwazYelNcI9qDCDkcn1cCX0vjnPmjQAUqYdtpPjZ1Kd0gh/rQETNon+dULW1j753
3h7tyS5VAw3S73voqO3rZzl+kuiM2nfcY2LhnBGOsAF5hk42WzVvTMgb2eRwUull
TnzWxKsz92HU4u+YqBUduw==
-----END PRIVATE KEY-----
";

/// The public half of `PRIVATE_KEY`, base64url without padding.
const MODULUS: &str = "jm-7a6iW_lsag0Pe2jeUPdzZSQp_bnClto4ycmRKu70VGRucVdeyg9AVIvDhsNxdkcdVYYPgJw9yKsbT_tUsSoMy6s8eAK5DnhNi2pzOH272bJM6hIvwEOctM4MG5ERtB8eyI1A5M4pMHt3oCl0KBsdJ7aFVcvKO7AD3xkvJGrbDpD-VFXjd2n8sZq5YsrRMZXMrTjmxZsE-adjARObNyXTRdfzvcTeiqumMpN4j7FrcmnGQ7egnmMPSNiaZ49L_uFZpsE4sXu-EtUHhpxpYDMASx0HN2VBvTggiXi09ob29N4GqIMe3V7-DttLeekfzlAdMMnQNHLFkowE0W_wzVQ";
const EXPONENT: &str = "AQAB";
/// The ID of the one key the test issuer publishes.
const KID: &str = "test";

#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub form: HashMap<String, String>,
}

/// What the issuer answers anything but its keys and configuration with.
#[derive(Debug, Clone)]
struct Reply {
    status: u16,
    body: String,
}

pub(crate) struct Issuer {
    pub url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    reply: Arc<Mutex<Reply>>,
    task: tokio::task::JoinHandle<()>,
}

impl Issuer {
    pub async fn start(status: u16, body: Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let reply = Arc::new(Mutex::new(Reply {
            status,
            body: body.to_string(),
        }));
        let answering = reply.clone();
        let jwks = serde_json::to_string(&jwks()).unwrap();
        let configuration = json!({
            "revocation_endpoint": format!("{url}/revoke"),
        })
        .to_string();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let mut request_line = line.split_whitespace();
                let method = request_line.next().unwrap_or("").to_string();
                let path = request_line.next().unwrap_or("").to_string();
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    stream.read_line(&mut header).await.unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut form = vec![0; length];
                stream.read_exact(&mut form).await.unwrap();
                let form = url::form_urlencoded::parse(&form).into_owned().collect();
                seen.lock().unwrap().push(Request {
                    method: method.clone(),
                    path: path.clone(),
                    form,
                });
                let (status, body) = match (method.as_str(), path.as_str()) {
                    ("GET", "/.well-known/jwks.json") => (200, jwks.clone()),
                    ("GET", "/.well-known/openid-configuration") => (200, configuration.clone()),
                    _ => {
                        let reply = answering.lock().unwrap().clone();
                        (reply.status, reply.body)
                    }
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.get_mut().write_all(response.as_bytes()).await;
            }
        });
        Self {
            url,
            requests,
            reply,
            task,
        }
    }

    /// Answers the next request with `status` and `body`: a login's token
    /// endpoint answer is only known once its nonce is.
    pub fn respond(&self, status: u16, body: Value) {
        *self.reply.lock().unwrap() = Reply {
            status,
            body: body.to_string(),
        };
    }

    /// Everything the issuer was asked, including the keys a login fetches.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// What was POSTed, which excludes the keys and configuration.
    pub fn posts(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|request| request.method == "POST")
            .collect()
    }
}

impl Drop for Issuer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The JWKS the test issuer publishes.
pub(crate) fn jwks() -> Jwks {
    serde_json::from_value(json!({
        "keys": [{
            "kty": "RSA",
            "kid": KID,
            "use": "sig",
            "alg": "RS256",
            "n": MODULUS,
            "e": EXPONENT,
        }],
    }))
    .unwrap()
}

/// An ID token signed as the test issuer signs one, with `claims` beside
/// the claims every ID token carries.
pub(crate) fn signed(claims: Value, issuer: &str, audience: &str, nonce: &str) -> String {
    let mut payload: Map<String, Value> = claims.as_object().cloned().unwrap_or_default();
    payload.insert("iss".into(), issuer.into());
    payload.insert("aud".into(), audience.into());
    payload.insert("nonce".into(), nonce.into());
    payload
        .entry("sub")
        .or_insert_with(|| Value::String("user".into()));
    payload
        .entry("exp")
        .or_insert_with(|| Value::Number((crate::store::now_ms() / 1000 + 3600).into()));
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.into());
    jsonwebtoken::encode(
        &header,
        &Value::Object(payload),
        &EncodingKey::from_rsa_pem(PRIVATE_KEY.as_bytes()).unwrap(),
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_published_key_checks_a_signed_token() {
        let signed = signed(
            json!({"sub": "user"}),
            "https://auth.example",
            "client",
            "nonce",
        );
        let mut validation = jsonwebtoken::Validation::new(Algorithm::RS256);
        validation.set_issuer(&["https://auth.example"]);
        validation.set_audience(&["client"]);
        let key = jsonwebtoken::DecodingKey::from_jwk(&jwks().keys[0]).unwrap();
        let claims = jsonwebtoken::decode::<Value>(&signed, &key, &validation)
            .expect("the public half matches the private half")
            .claims;
        assert_eq!(claims["nonce"], "nonce");
    }
}
