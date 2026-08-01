fn parse_form(body: &[u8]) -> Result<BTreeMap<String, String>> {
    let body = std::str::from_utf8(body).map_err(|_| Error::Request)?;
    let mut output = BTreeMap::new();
    for pair in body.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if output
            .insert(percent_decode(key)?, percent_decode(value)?)
            .is_some()
        {
            return Err(Error::Request);
        }
    }
    Ok(output)
}

fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let high = hex(bytes[index + 1]).ok_or(Error::Request)?;
                let low = hex(bytes[index + 2]).ok_or(Error::Request)?;
                output.push((high << 4) | low);
                index += 3;
            }
            b'%' => return Err(Error::Request),
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(output).map_err(|_| Error::Request)
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn field(form: &BTreeMap<String, String>, key: &str) -> Result<String> {
    form.get(key)
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or(Error::Request)
}

fn id_field(form: &BTreeMap<String, String>, key: &str) -> Result<String> {
    let value = field(form, key)?;
    identifier(key, &value).map_err(|_| Error::Request)
}

#[derive(Clone, Debug, Serialize)]
struct ContextMessage {
    user_id: Option<String>,
    ts: String,
    text: String,
}

#[derive(Debug, Deserialize)]
struct HistoryResponse {
    ok: bool,
    #[serde(default)]
    messages: Vec<HistoryMessage>,
}

#[derive(Debug, Deserialize)]
struct HistoryMessage {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: String,
    ts: String,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsergroupsResponse {
    ok: bool,
    #[serde(default)]
    usergroups: Vec<Usergroup>,
}

#[derive(Debug, Deserialize)]
struct Usergroup {
    id: String,
    #[serde(default)]
    users: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SlackResponse {
    ok: bool,
    #[serde(default)]
    ts: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JobResponse {
    job: Job,
}

#[derive(Debug, Deserialize)]
struct Job {
    id: String,
}

#[derive(Debug, Deserialize)]
struct WorkflowResponse {
    workflow: Workflow,
}

#[derive(Debug, Deserialize)]
struct Workflow {
    plan: WorkflowPlan,
}

#[derive(Debug, Deserialize)]
struct WorkflowPlan {
    id: String,
    #[serde(default)]
    assignments: Vec<Assignment>,
}

#[derive(Debug, Deserialize)]
struct Assignment {
    agent_key: String,
}

#[derive(Clone)]
struct App {
    config: Config,
    client: Client,
    registry: SlackProjectRegistry,
    bindings: BTreeMap<(String, String), ChannelProjectBinding>,
    capacity: Arc<Semaphore>,
}

