/// Slack sends roughly a dozen fields. A ceiling keeps a 1 MiB body of empty
/// pairs from turning into hundreds of thousands of map entries — the body is
/// signed, so this is only reachable by the installed app, but the parse runs
/// twice per request and there is no reason to leave the amplification there.
const MAX_FORM_FIELDS: usize = 64;

fn parse_form(body: &[u8]) -> Result<BTreeMap<String, String>> {
    let body = std::str::from_utf8(body).map_err(|_| Error::Request)?;
    let mut output = BTreeMap::new();
    for pair in body.split('&').filter(|pair| !pair.is_empty()) {
        if output.len() >= MAX_FORM_FIELDS {
            return Err(Error::Request);
        }
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


#[cfg(test)]
mod form_parser_hardening_tests {
    use super::*;

    #[test]
    fn a_flood_of_empty_pairs_is_refused_rather_than_allocated() {
        let distinct = (0..=MAX_FORM_FIELDS)
            .map(|index| format!("k{index}=v"))
            .collect::<Vec<_>>()
            .join("&");
        assert!(
            parse_form(distinct.as_bytes()).is_err(),
            "more than {MAX_FORM_FIELDS} fields must be refused"
        );
    }

    #[test]
    fn a_realistic_slack_envelope_still_parses() {
        let body = concat!(
            "token=x&team_id=T01B3C83PMK&team_domain=oresoftware-workspace",
            "&channel_id=C1&channel_name=ores&user_id=U1&user_name=alex",
            "&command=%2Fx-ores-claude&text=fix+DEN-1041&api_app_id=A0BMBAMM5NJ",
            "&response_url=https%3A%2F%2Fhooks.slack.com%2Fx&trigger_id=t1"
        );
        let form = parse_form(body.as_bytes()).expect("a real envelope must parse");
        assert_eq!(form.get("command").map(String::as_str), Some("/x-ores-claude"));
        assert_eq!(form.get("api_app_id").map(String::as_str), Some("A0BMBAMM5NJ"));
        assert!(form.len() < MAX_FORM_FIELDS);
    }

    #[test]
    fn a_truncated_escape_is_rejected_not_silently_kept() {
        assert!(parse_form(b"command=%2").is_err());
        assert!(parse_form(b"command=%").is_err());
        assert!(parse_form(b"command=%zz").is_err());
    }

    #[test]
    fn decoded_key_collisions_cannot_smuggle_a_second_team_id() {
        assert!(parse_form(b"team_id=T1&team%5Fid=T2").is_err());
    }
}
