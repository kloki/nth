//! A stand-in for auth.openai.com's token endpoint: answers every request
//! with one canned reply and keeps what was asked.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub path: String,
    pub form: HashMap<String, String>,
}

pub(crate) struct Issuer {
    pub url: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Issuer {
    pub async fn start(status: u16, body: serde_json::Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let body = body.to_string();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
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
                seen.lock().unwrap().push(Request { path, form });
                let reply = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.get_mut().write_all(reply.as_bytes()).await;
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Issuer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
