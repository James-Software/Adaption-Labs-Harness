use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Client as Http, RequestBuilder, Response as HttpResponse};
use serde::Deserialize;
use serde_json::Value;

const API: &str = "https://api.prod.adaptionlabs.ai/api/v1";

pub struct Client {
    http: Http,
    runtime: tokio::runtime::Runtime,
    base: String,
    auth: HeaderValue,
}

#[derive(Deserialize)]
pub struct Response {
    pub status: String,
    pub output: Vec<Value>,
    #[serde(default)]
    pub output_text: String,
}

#[derive(Deserialize)]
struct ModelList {
    data: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: String,
}

impl Client {
    pub fn new(key: &str) -> Result<Self> {
        Self::with_base(API, key)
    }

    fn with_base(base: &str, key: &str) -> Result<Self> {
        ensure!(!key.trim().is_empty(), "ADAPTION_API_KEY is empty");
        let mut auth =
            HeaderValue::from_str(&format!("Bearer {key}")).context("invalid ADAPTION_API_KEY")?;
        auth.set_sensitive(true);
        Ok(Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            http: Http::builder()
                .connect_timeout(Duration::from_secs(15))
                .timeout(Duration::from_secs(180))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base: base.to_owned(),
            auth,
        })
    }

    pub fn models(&self) -> Result<Vec<String>> {
        self.run(async {
            let response = self
                .send(self.http.get(format!("{}/models", self.base)))
                .await?;
            let mut models: Vec<_> = response
                .json::<ModelList>()
                .await?
                .data
                .into_iter()
                .map(|model| model.id)
                .collect();
            models.sort_unstable();
            models.dedup();
            Ok(models)
        })
    }

    pub fn respond(
        &self,
        body: &Value,
        mut on_text: impl FnMut(&str) -> Result<()>,
    ) -> Result<Response> {
        self.run(async {
            let mut response = self
                .send(
                    self.http
                        .post(format!("{}/responses", self.base))
                        .header("Accept", "text/event-stream")
                        .json(body),
                )
                .await?;
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            if content_type.starts_with("application/json") {
                let body = read_body(response, MAX_EVENT).await?;
                return completed(&serde_json::from_slice(&body)?);
            }
            ensure!(
                content_type.starts_with("text/event-stream"),
                "expected an Adaption event stream"
            );
            let mut stream = EventStream::default();
            while let Some(chunk) = response
                .chunk()
                .await
                .context("reading Adaption event stream")?
            {
                if let Some(response) = stream.push(&chunk, &mut on_text)? {
                    return Ok(response);
                }
            }
            stream
                .push(b"\n\n", &mut on_text)?
                .context("Adaption stream ended before a completed response")
        })
    }

    fn run<T>(&self, request: impl Future<Output = Result<T>>) -> Result<T> {
        self.runtime
            .block_on(interruptible(request, crate::tools::check_interrupt))
    }

    async fn send(&self, request: RequestBuilder) -> Result<HttpResponse> {
        let response = request
            .header(AUTHORIZATION, self.auth.clone())
            .send()
            .await
            .context("request to Adaption failed")?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }

        let body = read_body(response, 8192).await.unwrap_or_default();
        let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let message = value
            .pointer("/error/message")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("request failed");
        bail!("Adaption returned HTTP {status}: {message}");
    }
}

const MAX_EVENT: usize = 2 * 1024 * 1024;

async fn interruptible<T>(
    request: impl Future<Output = Result<T>>,
    mut check_interrupt: impl FnMut() -> Result<()>,
) -> Result<T> {
    let mut request = std::pin::pin!(request);
    loop {
        check_interrupt()?;
        if let Ok(result) = tokio::time::timeout(Duration::from_millis(100), &mut request).await {
            check_interrupt()?;
            return result;
        }
    }
}

async fn read_body(mut response: HttpResponse, limit: usize) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            body.len() + chunk.len() <= limit,
            "Adaption response body is too large"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Default)]
struct EventStream {
    line: Vec<u8>,
    data: String,
    event: String,
}

impl EventStream {
    fn push(
        &mut self,
        chunk: &[u8],
        on_text: &mut impl FnMut(&str) -> Result<()>,
    ) -> Result<Option<Response>> {
        for part in chunk.split_inclusive(|byte| *byte == b'\n') {
            ensure!(
                self.line.len() + part.len() <= MAX_EVENT,
                "Adaption event line is too large"
            );
            self.line.extend_from_slice(part);
            if part.last() != Some(&b'\n') {
                continue;
            }
            let field = std::str::from_utf8(&self.line)?.trim_end_matches(['\r', '\n']);
            if field.is_empty() {
                if self.data == "[DONE]" {
                    bail!("Adaption stream ended before a completed response");
                }
                if !self.data.is_empty() {
                    let value: Value = serde_json::from_str(&self.data)
                        .context("invalid Adaption stream event")?;
                    let kind = value
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or(&self.event);
                    match kind {
                        "response.output_text.delta" | "response.refusal.delta" => {
                            if let Some(delta) = value.get("delta").and_then(Value::as_str) {
                                on_text(delta)?;
                            }
                        }
                        "response.completed" => {
                            return Ok(Some(completed(
                                value
                                    .get("response")
                                    .context("completed event is missing its response")?,
                            )?));
                        }
                        "response.failed"
                        | "response.incomplete"
                        | "response.cancelled"
                        | "error" => {
                            bail!("Adaption {kind}: {}", failure(&value));
                        }
                        _ => {}
                    }
                }
                self.data.clear();
                self.event.clear();
            } else if let Some(payload) = field.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data
                    .push_str(payload.strip_prefix(' ').unwrap_or(payload));
                ensure!(self.data.len() <= MAX_EVENT, "Adaption event is too large");
            } else if let Some(name) = field.strip_prefix("event:") {
                self.event = name.trim_start().to_owned();
            }
            self.line.clear();
        }
        Ok(None)
    }
}

fn completed(value: &Value) -> Result<Response> {
    let response: Response =
        serde_json::from_value(value.clone()).context("invalid Adaption response")?;
    if response.status != "completed" {
        return Err(anyhow!(
            "Adaption response {}: {}",
            response.status,
            failure(value)
        ));
    }
    Ok(response)
}

fn failure(value: &Value) -> &str {
    for path in [
        "/response/error/message",
        "/error/message",
        "/message",
        "/response/incomplete_details/reason",
        "/incomplete_details/reason",
    ] {
        if let Some(message) = value.pointer(path).and_then(Value::as_str) {
            return message;
        }
    }
    "no details supplied"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn response_and_tool_output_round_trip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for round in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                let mut input = BufReader::new(&socket);
                let mut request = String::new();
                input.read_line(&mut request).unwrap();
                assert_eq!(request.trim_end(), "POST /responses HTTP/1.1");
                let mut length = 0;
                let mut authorized = false;
                loop {
                    let mut line = String::new();
                    input.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    authorized |= line.trim_end() == "authorization: Bearer test-key";
                }
                assert!(authorized);
                let mut body = vec![0; length];
                input.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["model"], "stack:coder");
                assert_eq!(body["agentic_memory"], true);
                assert_eq!(body["store"], false);
                assert_eq!(body["stream"], true);
                assert!(
                    body["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .all(|tool| tool["type"] == "function")
                );

                let (delta, outputs) = if round == 0 {
                    (
                        "",
                        serde_json::json!([{"type":"function_call","name":"list_files",
                        "call_id":"c1","arguments":"{}"}]),
                    )
                } else {
                    assert_eq!(body["input"][1]["type"], "function_call");
                    assert_eq!(body["input"][2]["call_id"], "c1");
                    assert_eq!(body["input"][2]["type"], "function_call_output");
                    assert!(
                        body["input"][2]["output"]
                            .as_str()
                            .unwrap()
                            .contains("hello.txt")
                    );
                    ("I found hello.txt.", serde_json::json!([]))
                };
                let event = serde_json::json!({"type":"response.completed",
                    "response":{"status":"completed","output":outputs}});
                let mut sse = String::new();
                if !delta.is_empty() {
                    sse.push_str(&format!(
                        "data: {}\r\n\r\n",
                        serde_json::json!({
                        "type":"response.output_text.delta","delta":delta})
                    ));
                }
                sse.push_str(&format!(
                    "event: response.completed\r\ndata: {event}\r\n\r\n"
                ));
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse.len(), sse).unwrap();
            }
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("hello.txt"), "hello").unwrap();
        let client = Client::with_base(&format!("http://{addr}"), "test-key").unwrap();
        let workspace = crate::workspace::Workspace::open(Some(root.path().into())).unwrap();
        let mut agent =
            crate::agent::Agent::new(client, "stack:coder".into(), Some(true), &workspace);
        let mut output = Vec::new();
        agent.run("List files.", &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "I found hello.txt.\n");
        server.join().unwrap();
    }

    #[test]
    fn stream_needs_a_completed_response() {
        let mut streamed = String::new();
        let result = EventStream::default().push(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\ndata: [DONE]\n\n".as_slice(),
            &mut |text| { streamed.push_str(text); Ok(()) },
        );
        assert!(result.is_err());
        assert_eq!(streamed, "partial");
    }

    #[test]
    fn a_stalled_response_can_be_interrupted() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let mut checks = 0;
        let result = runtime.block_on(interruptible(std::future::pending::<Result<()>>(), || {
            checks += 1;
            if checks > 1 {
                return Err(crate::terminal::Interrupted.into());
            }
            Ok(())
        }));
        assert!(result.unwrap_err().is::<crate::terminal::Interrupted>());
    }

    #[test]
    fn streaming_text_handles_utf8_split_across_chunks() {
        let event = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"café\"}\r\n\r\n";
        let split = event.find('é').unwrap() + 1;
        let mut stream = EventStream::default();
        let mut text = String::new();
        let mut on_text = |delta: &str| {
            text.push_str(delta);
            Ok(())
        };
        stream
            .push(&event.as_bytes()[..split], &mut on_text)
            .unwrap();
        stream
            .push(&event.as_bytes()[split..], &mut on_text)
            .unwrap();
        assert_eq!(text, "café");
    }
}
