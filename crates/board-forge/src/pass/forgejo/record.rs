//! Recording and replay of a Forgejo preview (docs/forges.md#recording-and-replay).
//! One log holds the forge's and the broker's reads in the order the pass
//! made them, so replay goes through the same stages, bounds and clock.
use super::*;
use crate::forgejo::{NotFound, Response};

/// The `forge` a Forgejo replay fixture names; a GitHub one names none.
const FORGE: &str = "forgejo";

/// What a Forgejo read answered: its body and `X-Total-Count`, a 404, which
/// the pass tells apart from other failures, or another failure's message.
fn forge_answer(read: &Result<Response>) -> Value {
    match read {
        Ok(response) => json!({"body": response.body, "total": response.total}),
        Err(error) if error.is::<NotFound>() => json!({"status": 404}),
        Err(error) => json!({"error": format!("{error:#}")}),
    }
}

fn broker_answer(read: &Result<Value>) -> Value {
    match read {
        Ok(body) => json!({"body": body}),
        Err(error) => json!({"error": format!("{error:#}")}),
    }
}

/// Reads through `forge` and `broker`, keeping each request and its answer,
/// every one in order. It keeps URLs and bodies, never a request's headers,
/// and makes no write: the transport's default `send` and `write` refuse.
pub struct Recorder<'a, C, B> {
    forge: &'a C,
    broker: &'a B,
    instance: String,
    entries: RefCell<Vec<Value>>,
}

impl<'a, C, B> Recorder<'a, C, B> {
    pub fn new(forge: &'a C, broker: &'a B, board: &ForgejoConfig) -> Self {
        Self {
            forge,
            broker,
            instance: instance(board),
            entries: RefCell::new(vec![]),
        }
    }

    fn keep(&self, request: Value, answer: Value) {
        (self.entries.borrow_mut()).push(json!({"request": request, "answer": answer}));
    }

    /// The fixture: the GitHub replay's, naming the forge and its instance.
    pub fn fixture(&self, now: OffsetDateTime) -> Result<String> {
        let head = json!({"schema": REPLAY_SCHEMA, "forge": FORGE,
            "instance": self.instance, "now": now.format(&Rfc3339)?});
        Ok(fixture_text(&head, &self.entries.borrow()))
    }
}

impl<C: ForgejoTransport, B> ForgejoTransport for Recorder<'_, C, B> {
    fn get(&self, url: &url::Url) -> Result<Response> {
        let read = self.forge.get(url);
        self.keep(json!({"get": url.as_str()}), forge_answer(&read));
        read
    }
}

impl<C, B: Broker> Broker for Recorder<'_, C, B> {
    fn usage(&self, url: &str) -> Result<Value> {
        let read = self.broker.usage(url);
        self.keep(broker_key(USAGE_PATH), broker_answer(&read));
        read
    }

    fn grant(&self, url: &str) -> Result<Value> {
        let read = self.broker.grant(url);
        self.keep(broker_key(GRANT_PATH), broker_answer(&read));
        read
    }
}

/// A recorded Forgejo preview ([`Recorder`]) at its clock. Each read must be
/// the next one recorded; after one that is not, every read fails, and
/// [`Replay::finish`] says the pass diverged. It never writes.
pub struct Replay {
    pub now: OffsetDateTime,
    requests: Vec<Value>,
    /// The next request, or none once the pass diverged.
    next: Cell<Option<usize>>,
}

impl Replay {
    /// A fixture of `board`'s instance, refusing a GitHub one or another
    /// instance's: its URLs would never match, and its decisions are not
    /// this board's.
    pub fn from_json(value: &Value, board: &ForgejoConfig) -> Result<Self> {
        ensure!(
            value["schema"] == REPLAY_SCHEMA,
            "not a pass replay (expected {REPLAY_SCHEMA})"
        );
        ensure!(
            value["forge"] == FORGE,
            "not a Forgejo pass replay: a GitHub recording replays only with a GitHub board"
        );
        let recorded = value["instance"].as_str().unwrap_or_default();
        ensure!(
            recorded == instance(board),
            "the replay was recorded on {recorded:?}, not this board.url {:?}",
            instance(board)
        );
        let now = OffsetDateTime::parse(text(value, "/now"), &Rfc3339).context("replay now")?;
        let requests = value["requests"]
            .as_array()
            .context("replay has no requests")?
            .clone();
        Ok(Self {
            now,
            requests,
            next: Cell::new(Some(0)),
        })
    }

    fn answer(&self, request: Value) -> Result<&Value> {
        let n = self
            .next
            .take()
            .context("the pass diverged from the replay")?;
        let entry = self
            .requests
            .get(n)
            .with_context(|| format!("the replay did not record {request}"))?;
        ensure!(
            entry["request"] == request,
            "the replay recorded {} next, not {request}",
            entry["request"]
        );
        self.next.set(Some(n + 1));
        Ok(&entry["answer"])
    }

    /// Fails unless the pass read every recorded request, and only those.
    pub fn finish(&self) -> Result<()> {
        match self.next.get() {
            Some(n) if n == self.requests.len() => Ok(()),
            Some(n) => bail!(
                "the pass read {n} of the replay's {} requests",
                self.requests.len()
            ),
            None => bail!("the pass diverged from the replay"),
        }
    }
}

/// `answer`'s `body`, or the failure it recorded.
fn recorded_body(answer: &Value) -> Result<Value> {
    if let Some(error) = answer.get("error") {
        bail!("{}", error.as_str().unwrap_or("recorded error"));
    }
    answer
        .get("body")
        .cloned()
        .context("recorded answer has no body")
}

impl ForgejoTransport for Replay {
    fn get(&self, url: &url::Url) -> Result<Response> {
        let answer = self.answer(json!({"get": url.as_str()}))?;
        if answer["status"] == 404 {
            return Err(NotFound.into());
        }
        Ok(Response {
            body: recorded_body(answer)?,
            total: answer["total"].as_u64(),
        })
    }
}

impl Broker for Replay {
    fn usage(&self, _: &str) -> Result<Value> {
        recorded_body(self.answer(broker_key(USAGE_PATH))?)
    }

    fn grant(&self, _: &str) -> Result<Value> {
        recorded_body(self.answer(broker_key(GRANT_PATH))?)
    }
}
