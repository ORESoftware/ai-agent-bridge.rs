fn slack_api_base_url(value: &str) -> Result<String> {
    let normalized = service_url(value)?;
    let url = Url::parse(&normalized)
        .map_err(|_| Error::Config("invalid Slack API base URL".into()))?;

    if url_host_is_loopback(&url) {
        return Ok(normalized);
    }

    if url.scheme() != "https"
        || !url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("slack.com"))
        || url.path() != "/api/"
    {
        return Err(Error::Config(
            "SLACK_API_BASE_URL must be https://slack.com/api/ or a loopback test URL".into(),
        ));
    }

    Ok(normalized)
}
